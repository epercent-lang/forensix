//! Read-only access to disk images and partition discovery.
//!
//! The source is never opened for writing. All reads go through [`Source`].

#[cfg(windows)]
use anyhow::Context;
use anyhow::{Result, bail};
use std::fs::File;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::FileExt;
#[cfg(windows)]
use std::os::windows::fs::FileExt;

/// A read-only byte source (raw image file or device).
pub struct Source {
    file: File,
    len: u64,
    /// Non-zero for raw Windows disks (`\\.\PhysicalDriveN`): reads there must
    /// start and end on sector boundaries, so they go through an aligned buffer.
    align: u64,
    /// Sectors that could not be read and were returned as zeros.
    bad: std::sync::atomic::AtomicU64,
    /// Read-ahead window for small device reads (raw disks are not cached by
    /// the OS, and parsers read headers a few bytes at a time).
    cache: std::sync::Mutex<(u64, Vec<u8>)>,
    /// Recently read 4 MiB blocks, while several scanners go over the same
    /// part of the source one after another (see multi.rs).
    blocks: std::sync::Mutex<Option<Blocks>>,
    /// Bytes actually requested from the file or device.
    raw_bytes: std::sync::atomic::AtomicU64,
}

const BLOCK: u64 = 4 << 20;

struct Blocks {
    cap: usize,
    map: std::collections::HashMap<u64, Vec<u8>>,
    order: std::collections::VecDeque<u64>,
}

const WINDOW: u64 = 1 << 20;

/// True for Windows device paths such as `\\.\PhysicalDrive3`.
pub fn is_device_path(path: &Path) -> bool {
    let s = path.to_string_lossy();
    s.starts_with(r"\\.\") || s.starts_with("//./")
}

impl Source {
    pub fn open(path: &Path) -> Result<Self> {
        // `File::open` opens read-only (GENERIC_READ on Windows, with the
        // default share mode so a mounted volume on the disk stays usable).
        let file = File::open(path).map_err(|e| {
            let mut m = format!("cannot open {}: {e}", path.display());
            if is_device_path(path) && e.kind() == std::io::ErrorKind::PermissionDenied {
                m.push_str(" (물리 디스크는 관리자 권한으로 실행해야 읽을 수 있습니다)");
            }
            anyhow::anyhow!(m)
        })?;
        if is_device_path(path) {
            #[cfg(windows)]
            {
                let g = win::geometry(&file)
                    .with_context(|| format!("cannot read the size of {}", path.display()))?;
                let align = g.sector.max(512).max(4096);
                return Ok(Self {
                    file,
                    len: g.size,
                    align,
                    bad: Default::default(),
                    cache: Default::default(),
                    blocks: Default::default(),
                    raw_bytes: Default::default(),
                });
            }
        }
        let mut len = file.metadata()?.len();
        if len == 0 {
            // Block devices report 0 length in metadata; seek to the end instead.
            use std::io::{Seek, SeekFrom};
            let mut f = &file;
            len = f.seek(SeekFrom::End(0))?;
        }
        Ok(Self {
            file,
            len,
            align: 0,
            bad: Default::default(),
            cache: Default::default(),
            blocks: Default::default(),
            raw_bytes: Default::default(),
        })
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    /// Number of 512-byte sectors that failed to read and were zero-filled.
    pub fn bad_sectors(&self) -> u64 {
        self.bad.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Bytes requested from the file or device so far (re-reads included).
    pub fn bytes_read(&self) -> u64 {
        self.raw_bytes.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Keeps up to `mib` MiB of recently read blocks in memory (0: off).
    pub fn set_block_cache(&self, mib: u64) {
        *self.blocks.lock().unwrap() = (mib > 0).then(|| Blocks {
            cap: (mib * (1 << 20) / BLOCK).max(2) as usize,
            map: Default::default(),
            order: Default::default(),
        });
    }

    fn read_blocks(&self, bl: &mut Blocks, offset: u64, out: &mut [u8]) -> Result<usize> {
        let mut done = 0;
        while done < out.len() {
            let pos = offset + done as u64;
            let idx = pos / BLOCK;
            if !bl.map.contains_key(&idx) {
                let base = idx * BLOCK;
                let mut v = vec![0u8; BLOCK.min(self.len - base) as usize];
                let n = self.read_direct(base, &mut v)?;
                v.truncate(n);
                while bl.order.len() >= bl.cap {
                    let old = bl.order.pop_front().unwrap();
                    bl.map.remove(&old);
                }
                bl.order.push_back(idx);
                bl.map.insert(idx, v);
            }
            let v = &bl.map[&idx];
            let from = (pos - idx * BLOCK) as usize;
            let n = (out.len() - done).min(v.len().saturating_sub(from));
            if n == 0 {
                break;
            }
            out[done..done + n].copy_from_slice(&v[from..from + n]);
            done += n;
        }
        Ok(done)
    }

    fn raw_read(&self, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
        self.raw_bytes
            .fetch_add(buf.len() as u64, std::sync::atomic::Ordering::Relaxed);
        #[cfg(unix)]
        return self.file.read_at(buf, offset);
        #[cfg(windows)]
        return self.file.seek_read(buf, offset);
    }

    /// Reads up to `buf.len()` bytes at `offset`. Returns the number of bytes read
    /// (shorter only at the end of the source).
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if offset >= self.len {
            return Ok(0);
        }
        let want = buf.len().min((self.len - offset) as usize);
        if let Some(bl) = self.blocks.lock().unwrap().as_mut() {
            return self.read_blocks(bl, offset, &mut buf[..want]);
        }
        self.read_direct(offset, &mut buf[..want])
    }

    fn read_direct(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let want = buf.len();
        if self.align != 0 {
            if (want as u64) < WINDOW / 4 {
                return self.read_cached(offset, &mut buf[..want]);
            }
            return self.read_aligned(offset, &mut buf[..want]);
        }
        let mut done = 0;
        while done < want {
            let n = self.raw_read(&mut buf[done..want], offset + done as u64)?;
            if n == 0 {
                break;
            }
            done += n;
        }
        Ok(done)
    }

    fn read_cached(&self, offset: u64, out: &mut [u8]) -> Result<usize> {
        let mut c = self.cache.lock().unwrap();
        let end = offset + out.len() as u64;
        if !(offset >= c.0 && end <= c.0 + c.1.len() as u64) {
            let base = offset - offset % WINDOW;
            let len = (WINDOW * 2).min(self.len - base) as usize;
            let mut v = vec![0u8; len];
            let n = self.read_aligned(base, &mut v)?;
            v.truncate(n);
            *c = (base, v);
        }
        let from = (offset - c.0) as usize;
        let n = out.len().min(c.1.len().saturating_sub(from));
        out[..n].copy_from_slice(&c.1[from..from + n]);
        Ok(n)
    }

    /// Device read: sector-aligned offset, length and buffer. Unreadable
    /// sectors are zero-filled and counted instead of failing the whole scan.
    fn read_aligned(&self, offset: u64, out: &mut [u8]) -> Result<usize> {
        const CHUNK: u64 = 4 << 20;
        let a = self.align;
        let mut done = 0usize;
        let cap = ((out.len() as u64 + a).div_ceil(a) * a).min(CHUNK);
        let mut store = vec![0u8; (cap + a) as usize];
        let shift = store.as_ptr().align_offset(a as usize);
        let bounce = &mut store[shift..shift + cap as usize];
        while done < out.len() {
            let pos = offset + done as u64;
            let base = pos - pos % a;
            let skip = (pos - base) as usize;
            let need = (skip + out.len() - done) as u64;
            // Round up to the alignment but never past the end of the disk
            // (disk sizes are whole 512-byte sectors).
            let mut len = need.div_ceil(a) * a;
            len = len.min(cap).min(self.len - base);
            let len = len as usize;
            let got = self.read_device(base, &mut bounce[..len]);
            if got <= skip {
                break;
            }
            let n = (got - skip).min(out.len() - done);
            out[done..done + n].copy_from_slice(&bounce[skip..skip + n]);
            done += n;
        }
        Ok(done)
    }

    fn read_device(&self, base: u64, buf: &mut [u8]) -> usize {
        let mut done = 0;
        while done < buf.len() {
            match self.raw_read(&mut buf[done..], base + done as u64) {
                Ok(0) => break,
                Ok(n) => done += n,
                Err(_) => {
                    // Retry sector by sector; zero-fill the ones that fail.
                    let end = buf.len();
                    let mut p = done;
                    while p < end {
                        let s = 512.min(end - p);
                        if self.raw_read(&mut buf[p..p + s], base + p as u64).is_err() {
                            buf[p..p + s].fill(0);
                            self.bad.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        p += s;
                    }
                    return end;
                }
            }
        }
        done
    }

    pub fn read_vec(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut v = vec![0u8; len];
        let n = self.read_at(offset, &mut v)?;
        v.truncate(n);
        Ok(v)
    }

    pub fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let n = self.read_at(offset, buf)?;
        if n != buf.len() {
            bail!("short read at offset {offset:#x}");
        }
        Ok(())
    }
}

/// A physical disk as Windows numbers it (Disk Management "디스크 N").
#[derive(Debug, Clone, serde::Serialize)]
pub struct Disk {
    pub number: u32,
    pub path: String,
    pub size: u64,
    pub model: String,
    pub removable: bool,
    /// Drive letters of volumes on this disk.
    pub letters: Vec<char>,
    /// Whether this process may read it (needs administrator rights).
    pub readable: bool,
}

#[cfg(windows)]
pub fn list_disks() -> Vec<Disk> {
    win::list_disks()
}

#[cfg(not(windows))]
pub fn list_disks() -> Vec<Disk> {
    vec![]
}

/// Disk number of a drive letter's volume, e.g. `'E'` → 3.
#[cfg(windows)]
pub fn disk_of_letter(letter: char) -> Option<u32> {
    win::disk_of_letter(letter)
}

#[cfg(not(windows))]
pub fn disk_of_letter(_letter: char) -> Option<u32> {
    None
}

/// Free bytes for the caller on the volume holding `path`.
#[cfg(windows)]
pub fn free_space(path: &Path) -> Option<u64> {
    win::free_space(path)
}

#[cfg(not(windows))]
pub fn free_space(_path: &Path) -> Option<u64> {
    None
}

/// Disk number from `\\.\PhysicalDriveN`.
pub fn device_number(path: &Path) -> Option<u32> {
    let s = path.to_string_lossy().to_ascii_lowercase();
    let i = s.find("physicaldrive")?;
    s[i + 13..].parse().ok()
}

#[cfg(windows)]
mod win {
    //! Win32 disk IOCTLs (documented in the Windows SDK headers winioctl.h).
    use super::Disk;
    use std::ffi::c_void;
    use std::fs::{File, OpenOptions};
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;

    // CTL_CODE(DeviceType, Function, Method, Access) values from winioctl.h.
    const IOCTL_DISK_GET_DRIVE_GEOMETRY_EX: u32 = 0x0007_00A0;
    const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D_1400;
    const IOCTL_STORAGE_GET_DEVICE_NUMBER: u32 = 0x002D_1080;
    const FILE_SHARE_READ_WRITE: u32 = 0x1 | 0x2;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(
            dir: *const u16,
            avail: *mut u64,
            total: *mut u64,
            free: *mut u64,
        ) -> i32;
        fn DeviceIoControl(
            h: *mut c_void,
            code: u32,
            inb: *const c_void,
            insz: u32,
            outb: *mut c_void,
            outsz: u32,
            ret: *mut u32,
            ov: *mut c_void,
        ) -> i32;
    }

    fn ioctl(f: &File, code: u32, input: &[u8], out: &mut [u8]) -> Option<usize> {
        let mut ret = 0u32;
        // SAFETY: buffers are valid for the given sizes; synchronous call.
        let ok = unsafe {
            DeviceIoControl(
                f.as_raw_handle(),
                code,
                input.as_ptr() as *const c_void,
                input.len() as u32,
                out.as_mut_ptr() as *mut c_void,
                out.len() as u32,
                &mut ret,
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some(ret as usize)
    }

    /// Opens a device with no data access: enough for the IOCTLs below and
    /// allowed without administrator rights.
    fn open_query(path: &str) -> Option<File> {
        OpenOptions::new()
            .access_mode(0)
            .share_mode(FILE_SHARE_READ_WRITE)
            .open(path)
            .ok()
    }

    pub struct Geometry {
        pub size: u64,
        pub sector: u64,
    }

    pub fn geometry(f: &File) -> std::io::Result<Geometry> {
        // DISK_GEOMETRY_EX: DISK_GEOMETRY (24 bytes, BytesPerSector at 20),
        // then DiskSize (LARGE_INTEGER) at 24.
        let mut out = [0u8; 256];
        let n = ioctl(f, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &[], &mut out)
            .ok_or_else(std::io::Error::last_os_error)?;
        if n < 32 {
            return Err(std::io::Error::other("short DISK_GEOMETRY_EX"));
        }
        Ok(Geometry {
            sector: u32::from_le_bytes(out[20..24].try_into().unwrap()) as u64,
            size: u64::from_le_bytes(out[24..32].try_into().unwrap()),
        })
    }

    fn device_number(f: &File) -> Option<u32> {
        // STORAGE_DEVICE_NUMBER { DeviceType, DeviceNumber, PartitionNumber }.
        let mut out = [0u8; 12];
        ioctl(f, IOCTL_STORAGE_GET_DEVICE_NUMBER, &[], &mut out)?;
        Some(u32::from_le_bytes(out[4..8].try_into().unwrap()))
    }

    pub fn free_space(path: &std::path::Path) -> Option<u64> {
        use std::os::windows::ffi::OsStrExt;
        let w: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        let (mut a, mut t, mut f) = (0u64, 0u64, 0u64);
        // SAFETY: NUL-terminated path, valid out pointers.
        let ok = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut a, &mut t, &mut f) };
        (ok != 0).then_some(a)
    }

    pub fn disk_of_letter(letter: char) -> Option<u32> {
        device_number(&open_query(&format!(r"\\.\{letter}:"))?)
    }

    fn describe(f: &File) -> (String, bool) {
        // STORAGE_PROPERTY_QUERY { StorageDeviceProperty = 0, PropertyStandardQuery = 0 }.
        let q = [0u8; 12];
        let mut out = vec![0u8; 1024];
        let Some(n) = ioctl(f, IOCTL_STORAGE_QUERY_PROPERTY, &q, &mut out) else {
            return (String::new(), false);
        };
        let out = &out[..n];
        // STORAGE_DEVICE_DESCRIPTOR: RemovableMedia at 10, VendorIdOffset at 12,
        // ProductIdOffset at 16 (offsets to NUL-terminated ASCII strings).
        let s = |at: usize| -> String {
            let Some(o) = out.get(at..at + 4) else {
                return String::new();
            };
            let o = u32::from_le_bytes(o.try_into().unwrap()) as usize;
            if o == 0 || o >= out.len() {
                return String::new();
            }
            let end = out[o..]
                .iter()
                .position(|&b| b == 0)
                .map_or(out.len(), |e| o + e);
            String::from_utf8_lossy(&out[o..end]).trim().to_string()
        };
        let model = format!("{} {}", s(12), s(16)).trim().to_string();
        (model, out.get(10).is_some_and(|&b| b != 0))
    }

    pub fn list_disks() -> Vec<Disk> {
        let letters: Vec<(char, u32)> = ('A'..='Z')
            .filter_map(|c| disk_of_letter(c).map(|n| (c, n)))
            .collect();
        let mut disks = vec![];
        for n in 0..64u32 {
            let path = format!(r"\\.\PhysicalDrive{n}");
            let Some(f) = open_query(&path) else { continue };
            let Ok(g) = geometry(&f) else { continue };
            let (model, removable) = describe(&f);
            disks.push(Disk {
                number: n,
                readable: File::open(&path).is_ok(),
                path,
                size: g.size,
                model,
                removable,
                letters: letters.iter().filter(|l| l.1 == n).map(|l| l.0).collect(),
            });
        }
        disks
    }
}

/// Kind of filesystem recognised from a volume boot record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum FsKind {
    Fat32,
    ExFat,
    Ntfs,
    /// ext2, ext3 or ext4.
    Ext,
    Xfs,
    Btrfs,
    /// HFS+ or HFSX.
    HfsPlus,
    Jfs,
    /// ReiserFS 3.5 / 3.6.
    Reiser,
    Apfs,
    Unknown,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Volume {
    pub offset: u64,
    pub length: u64,
    pub kind: FsKind,
}

pub fn detect_fs(boot: &[u8]) -> FsKind {
    if boot.len() < 512 {
        return FsKind::Unknown;
    }
    if &boot[3..11] == b"EXFAT   " {
        return FsKind::ExFat;
    }
    if &boot[3..11] == b"NTFS    " && boot[510] == 0x55 && boot[511] == 0xAA {
        return FsKind::Ntfs;
    }
    if &boot[82..90] == b"FAT32   " && boot[510] == 0x55 && boot[511] == 0xAA {
        return FsKind::Fat32;
    }
    FsKind::Unknown
}

/// File system at `offset`: boot-sector types first, then the ext super
/// block (at byte 1024 of the volume).
fn detect_at(src: &Source, offset: u64, boot: &[u8]) -> FsKind {
    let k = detect_fs(boot);
    if k != FsKind::Unknown {
        return k;
    }
    if crate::xfs::is_xfs(boot) {
        return FsKind::Xfs;
    }
    match src.read_vec(offset + 1024, 1024) {
        Ok(sb) if crate::ext4::is_ext(&sb) => return FsKind::Ext,
        Ok(sb) if crate::hfsplus::is_hfsplus(&sb) => return FsKind::HfsPlus,
        _ => {}
    }
    match src.read_vec(offset + 0x8000, 64) {
        Ok(sb) if crate::jfs::is_jfs(&sb) => return FsKind::Jfs,
        _ => {}
    }
    match src.read_vec(offset + 0x10000, 0x48) {
        Ok(sb) if crate::btrfs::is_btrfs(&sb) => FsKind::Btrfs,
        _ => FsKind::Unknown,
    }
}

/// Finds volumes: either the whole image is a volume (no partition table, common
/// for some cards after reformatting) or an MBR lists primary partitions.
pub fn find_volumes(src: &Source) -> Result<Vec<Volume>> {
    let mut s0 = [0u8; 512];
    src.read_exact_at(0, &mut s0)?;
    let kind = detect_at(src, 0, &s0);
    if kind != FsKind::Unknown {
        return Ok(vec![Volume {
            offset: 0,
            length: src.len(),
            kind,
        }]);
    }
    let mut vols = Vec::new();
    // GPT (UEFI specification, ch. 5): header at LBA 1, protective MBR type 0xEE.
    let mut h = [0u8; 512];
    if src.len() >= 1024 {
        src.read_exact_at(512, &mut h)?;
    }
    if &h[0..8] == b"EFI PART" {
        let lba = u64::from_le_bytes(h[72..80].try_into().unwrap());
        let n = u32::from_le_bytes(h[80..84].try_into().unwrap()).min(1024) as u64;
        let esz = u32::from_le_bytes(h[84..88].try_into().unwrap()) as u64;
        if (128..=4096).contains(&esz) {
            let tab = src.read_vec(lba * 512, (n * esz) as usize)?;
            for i in 0..n as usize {
                let e = &tab[i * esz as usize..(i + 1) * esz as usize];
                if e[0..16].iter().all(|&b| b == 0) {
                    continue;
                }
                let first = u64::from_le_bytes(e[32..40].try_into().unwrap());
                let last = u64::from_le_bytes(e[40..48].try_into().unwrap());
                let offset = first * 512;
                if last < first || offset >= src.len() {
                    continue;
                }
                let mut b = [0u8; 512];
                src.read_exact_at(offset, &mut b)?;
                vols.push(Volume {
                    offset,
                    length: ((last - first + 1) * 512).min(src.len() - offset),
                    kind: detect_at(src, offset, &b),
                });
            }
            return Ok(vols);
        }
    }
    if s0[510] == 0x55 && s0[511] == 0xAA {
        for i in 0..4 {
            let e = &s0[446 + i * 16..446 + (i + 1) * 16];
            let ptype = e[4];
            let lba = u32::from_le_bytes(e[8..12].try_into().unwrap()) as u64;
            let count = u32::from_le_bytes(e[12..16].try_into().unwrap()) as u64;
            if ptype == 0 || count == 0 || ptype == 0xEE {
                continue;
            }
            let offset = lba * 512;
            if offset >= src.len() {
                continue;
            }
            let mut b = [0u8; 512];
            src.read_exact_at(offset, &mut b)?;
            vols.push(Volume {
                offset,
                length: (count * 512).min(src.len() - offset),
                kind: detect_at(src, offset, &b),
            });
        }
    }
    Ok(vols)
}

pub fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
pub fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
pub fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
pub fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}
pub fn be64(b: &[u8], o: usize) -> u64 {
    u64::from_be_bytes(b[o..o + 8].try_into().unwrap())
}

#[cfg(test)]
mod device_tests {
    use super::*;

    #[test]
    fn aligned_reads_match_plain_reads() {
        let dir = std::env::temp_dir().join(format!("vr_align_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("img.bin");
        // 3 MiB + 1536: ends on a 512-byte sector but not on 4096.
        let data: Vec<u8> = (0..(3 << 20) + 1536u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        std::fs::write(&p, &data).unwrap();
        let plain = Source::open(&p).unwrap();
        let mut dev = Source::open(&p).unwrap();
        dev.align = 4096;
        let mut seed = 7u64;
        for i in 0..400 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let off = (seed >> 20) % (data.len() as u64 + 100);
            let len = if i % 5 == 0 {
                (seed >> 7) as usize % (6 << 20)
            } else {
                (seed >> 9) as usize % 9000
            };
            let mut a = vec![0u8; len];
            let mut b = vec![1u8; len];
            let na = plain.read_at(off, &mut a).unwrap();
            let nb = dev.read_at(off, &mut b).unwrap();
            assert_eq!(na, nb, "len at {off}+{len}");
            assert_eq!(a[..na], b[..nb], "data at {off}+{len}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn device_numbers() {
        assert_eq!(device_number(Path::new(r"\\.\PhysicalDrive3")), Some(3));
        assert!(is_device_path(Path::new(r"\\.\PhysicalDrive12")));
        assert!(!is_device_path(Path::new(r"E:\Claude\6456.raw")));
    }
}
