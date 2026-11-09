//! Read-only access to disk images and partition discovery.
//!
//! The source is never opened for writing. All reads go through [`Source`].

use anyhow::{Context, Result, bail};
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
}

impl Source {
    pub fn open(path: &Path) -> Result<Self> {
        // `File::open` opens read-only.
        let file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        let mut len = file.metadata()?.len();
        if len == 0 {
            // Block devices report 0 length in metadata; seek to the end instead.
            use std::io::{Seek, SeekFrom};
            let mut f = &file;
            len = f.seek(SeekFrom::End(0))?;
        }
        Ok(Self { file, len })
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    /// Reads up to `buf.len()` bytes at `offset`. Returns the number of bytes read
    /// (shorter only at the end of the source).
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if offset >= self.len {
            return Ok(0);
        }
        let want = buf.len().min((self.len - offset) as usize);
        let mut done = 0;
        while done < want {
            #[cfg(unix)]
            let n = self
                .file
                .read_at(&mut buf[done..want], offset + done as u64)?;
            #[cfg(windows)]
            let n = self
                .file
                .seek_read(&mut buf[done..want], offset + done as u64)?;
            if n == 0 {
                break;
            }
            done += n;
        }
        Ok(done)
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

/// Kind of filesystem recognised from a volume boot record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum FsKind {
    Fat32,
    ExFat,
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
    if &boot[82..90] == b"FAT32   " && boot[510] == 0x55 && boot[511] == 0xAA {
        return FsKind::Fat32;
    }
    FsKind::Unknown
}

/// Finds volumes: either the whole image is a volume (no partition table, common
/// for some cards after reformatting) or an MBR lists primary partitions.
pub fn find_volumes(src: &Source) -> Result<Vec<Volume>> {
    let mut s0 = [0u8; 512];
    src.read_exact_at(0, &mut s0)?;
    let kind = detect_fs(&s0);
    if kind != FsKind::Unknown {
        return Ok(vec![Volume {
            offset: 0,
            length: src.len(),
            kind,
        }]);
    }
    let mut vols = Vec::new();
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
                kind: detect_fs(&b),
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
