//! FAT32 and exFAT metadata: deleted directory entries and allocation maps.
//!
//! References: Microsoft "FAT32 File System Specification" (FAT: General
//! Overview of On-Disk Format) and Microsoft "exFAT File System Specification".
//! Only reading is implemented.

use crate::io::{FsKind, Source, Volume, le16, le32, le64};
use crate::ranges::Ranges;
use anyhow::{Result, bail};
use std::collections::HashSet;

#[derive(Debug, Clone, serde::Serialize)]
pub struct DeletedFile {
    pub path: String,
    pub size: u64,
    pub first_cluster: u32,
    /// Absolute byte extents in the source, in file order.
    pub extents: Vec<(u64, u64)>,
    /// How the extents were derived.
    pub method: &'static str,
    pub modified: Option<String>,
    pub created: Option<String>,
}

#[derive(Debug, Default)]
pub struct FsInfo {
    pub deleted: Vec<DeletedFile>,
    /// Files still in the directory tree (same record type; method says how
    /// their extents were read).
    pub live: Vec<DeletedFile>,
    /// Bytes in use: metadata areas plus allocated clusters (absolute).
    pub allocated: Ranges,
    pub live_files: usize,
    pub cluster_size: u64,
    pub cluster_count: u32,
}

/// Turns a cluster list into merged absolute byte extents covering `size` bytes.
fn extents_of(clusters: &[u32], size: u64, cs: u64, coff: impl Fn(u32) -> u64) -> Vec<(u64, u64)> {
    let mut ext: Vec<(u64, u64)> = vec![];
    let mut left = size;
    for &c in clusters {
        if left == 0 {
            break;
        }
        let s0 = coff(c);
        let l = left.min(cs);
        match ext.last_mut() {
            Some(x) if x.1 == s0 => x.1 += l,
            _ => ext.push((s0, s0 + l)),
        }
        left -= l;
    }
    ext
}

/// DOS date/time (FAT and exFAT timestamp layout) as ISO-8601 local time.
fn dos_time(date: u16, time: u16) -> Option<String> {
    if date == 0 {
        return None;
    }
    let (y, m, d) = (1980 + (date >> 9), (date >> 5) & 15, date & 31);
    let (hh, mm, ss) = (time >> 11, (time >> 5) & 63, (time & 31) * 2);
    if !(1..=12).contains(&m) || d == 0 || hh > 23 || mm > 59 || ss > 59 {
        return None;
    }
    Some(format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}"))
}

pub fn analyze(src: &Source, vol: &Volume) -> Result<Option<FsInfo>> {
    match vol.kind {
        FsKind::Fat32 => Fat32::open(src, vol)?.analyze().map(Some),
        FsKind::ExFat => ExFat::open(src, vol)?.analyze().map(Some),
        FsKind::Unknown => Ok(None),
    }
}

// ------------------------------------------------------------------ FAT32

struct Fat32<'a> {
    src: &'a Source,
    vol: u64,
    cluster_size: u64,
    data_start: u64,
    fat: Vec<u32>,
    root: u32,
}

impl<'a> Fat32<'a> {
    fn open(src: &'a Source, vol: &Volume) -> Result<Self> {
        let mut b = [0u8; 512];
        src.read_exact_at(vol.offset, &mut b)?;
        let bps = le16(&b, 11) as u64;
        let spc = b[13] as u64;
        let reserved = le16(&b, 14) as u64;
        let nfats = b[16] as u64;
        let fatsz = le32(&b, 36) as u64;
        let total = if le16(&b, 19) != 0 {
            le16(&b, 19) as u64
        } else {
            le32(&b, 32) as u64
        };
        if !matches!(bps, 512 | 1024 | 2048 | 4096)
            || spc == 0
            || !spc.is_power_of_two()
            || nfats == 0
            || fatsz == 0
        {
            bail!("invalid FAT32 boot sector");
        }
        let data_start_sec = reserved + nfats * fatsz;
        let clusters = (total.saturating_sub(data_start_sec) / spc) as usize;
        let raw = src.read_vec(vol.offset + reserved * bps, (fatsz * bps) as usize)?;
        let n = (clusters + 2).min(raw.len() / 4);
        let fat = (0..n).map(|i| le32(&raw, i * 4) & 0x0FFF_FFFF).collect();
        Ok(Self {
            src,
            vol: vol.offset,
            cluster_size: bps * spc,
            data_start: vol.offset + data_start_sec * bps,
            fat,
            root: le32(&b, 44),
        })
    }

    fn coff(&self, c: u32) -> u64 {
        self.data_start + (c as u64 - 2) * self.cluster_size
    }

    fn valid(&self, c: u32) -> bool {
        c >= 2 && (c as usize) < self.fat.len()
    }

    fn chain(&self, start: u32) -> Vec<u32> {
        let mut v = vec![];
        let mut seen = HashSet::new();
        let mut c = start;
        while self.valid(c) && seen.insert(c) {
            v.push(c);
            c = self.fat[c as usize];
            if c >= 0x0FFF_FFF8 {
                break;
            }
        }
        v
    }

    /// Contiguous extents for a deleted file. The FAT chain is zeroed on
    /// deletion, so assume consecutive clusters, but skip clusters that are
    /// now allocated to other files (they cannot hold this file's data unless
    /// overwritten).
    fn deleted_extents(&self, first: u32, size: u64) -> (Vec<(u64, u64)>, &'static str) {
        let need = size.div_ceil(self.cluster_size).max(1);
        let mut ext: Vec<(u64, u64)> = vec![];
        let mut got = 0u64;
        let mut c = first;
        let mut skipped = false;
        while got < need && self.valid(c) {
            if self.fat[c as usize] == 0 {
                let s = self.coff(c);
                let len = (size - got * self.cluster_size).min(self.cluster_size);
                match ext.last_mut() {
                    Some(l) if l.1 == s => l.1 += len,
                    _ => ext.push((s, s + len)),
                }
                got += 1;
            } else if c == first {
                // First cluster already reused: the start is overwritten.
                return (vec![], "first_cluster_reused");
            } else {
                skipped = true;
            }
            c += 1;
        }
        (
            ext,
            if skipped {
                "contiguous_free_clusters"
            } else {
                "contiguous"
            },
        )
    }

    fn analyze(self) -> Result<FsInfo> {
        let mut info = FsInfo {
            cluster_size: self.cluster_size,
            cluster_count: self.fat.len().saturating_sub(2) as u32,
            ..Default::default()
        };
        info.allocated.add(self.vol, self.data_start);
        for c in 2..self.fat.len() as u32 {
            if self.fat[c as usize] != 0 {
                let o = self.coff(c);
                info.allocated.add(o, o + self.cluster_size);
            }
        }
        info.allocated.normalize();

        // Walk directories: allocated ones by chain, deleted ones contiguously.
        let mut stack: Vec<(Vec<u32>, String, bool)> =
            vec![(self.chain(self.root), String::new(), false)];
        let mut visited = HashSet::new();
        while let Some((clusters, path, in_deleted)) = stack.pop() {
            if clusters.is_empty() || !visited.insert(clusters[0]) {
                continue;
            }
            let mut lfn: Vec<(u8, [u16; 13])> = vec![];
            'dir: for &c in &clusters {
                let data = self
                    .src
                    .read_vec(self.coff(c), self.cluster_size as usize)?;
                for e in data.chunks_exact(32) {
                    if e[0] == 0 {
                        break 'dir;
                    }
                    let attr = e[11];
                    if attr == 0x0F {
                        let mut chars = [0u16; 13];
                        for (k, off) in [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30]
                            .iter()
                            .enumerate()
                        {
                            chars[k] = le16(e, *off);
                        }
                        lfn.push((e[0], chars));
                        continue;
                    }
                    let long = take_lfn(&mut lfn);
                    if attr & 0x08 != 0 || &e[0..2] == b". " || &e[0..2] == b".." {
                        continue;
                    }
                    let deleted = e[0] == 0xE5 || in_deleted;
                    let name = long.unwrap_or_else(|| short_name(e, deleted));
                    let first = ((le16(e, 20) as u32) << 16) | le16(e, 26) as u32;
                    let size = le32(e, 28) as u64;
                    let full = format!("{path}/{name}");
                    if attr & 0x10 != 0 {
                        if self.valid(first) {
                            let cl = if deleted {
                                (first..first + 8).filter(|&x| self.valid(x)).collect()
                            } else {
                                self.chain(first)
                            };
                            // A deleted directory's own clusters must still look like one.
                            if !deleted || self.looks_like_dir(first)? {
                                stack.push((cl, full, deleted));
                            }
                        }
                        continue;
                    }
                    if !deleted {
                        info.live_files += 1;
                        let cl = if self.valid(first) {
                            self.chain(first)
                        } else {
                            vec![]
                        };
                        info.live.push(DeletedFile {
                            path: full,
                            size,
                            first_cluster: first,
                            extents: extents_of(&cl, size, self.cluster_size, |c| self.coff(c)),
                            method: "fat_chain",
                            modified: dos_time(le16(e, 24), le16(e, 22)),
                            created: dos_time(le16(e, 16), le16(e, 14)),
                        });
                        continue;
                    }
                    if size == 0 || !self.valid(first) {
                        continue;
                    }
                    let (extents, method) = self.deleted_extents(first, size);
                    info.deleted.push(DeletedFile {
                        path: full,
                        size,
                        first_cluster: first,
                        extents,
                        method,
                        modified: dos_time(le16(e, 24), le16(e, 22)),
                        created: dos_time(le16(e, 16), le16(e, 14)),
                    });
                }
            }
        }
        Ok(info)
    }

    fn looks_like_dir(&self, c: u32) -> Result<bool> {
        let d = self.src.read_vec(self.coff(c), 64)?;
        Ok(d.len() == 64 && &d[0..2] == b". " && &d[32..34] == b"..")
    }
}

fn take_lfn(lfn: &mut Vec<(u8, [u16; 13])>) -> Option<String> {
    if lfn.is_empty() {
        return None;
    }
    // Entries are stored last-part-first; deleted entries lose their ordinal
    // (first byte 0xE5) but keep their characters, so use storage order.
    let mut units = vec![];
    for (_, chars) in lfn.iter().rev() {
        for &u in chars {
            if u == 0 || u == 0xFFFF {
                break;
            }
            units.push(u);
        }
    }
    lfn.clear();
    let s = String::from_utf16_lossy(&units);
    (!s.is_empty()).then_some(s)
}

fn short_name(e: &[u8], deleted: bool) -> String {
    let mut base: Vec<u8> = e[0..8].to_vec();
    if deleted && base[0] == 0xE5 {
        base[0] = b'_'; // first character is lost on deletion
    }
    let mut b = String::from_utf8_lossy(&base).trim_end().to_string();
    let mut x = String::from_utf8_lossy(&e[8..11]).trim_end().to_string();
    // Windows NT case flags (byte 12): 0x08 lower-case base, 0x10 lower-case extension.
    if e[12] & 0x08 != 0 {
        b = b.to_lowercase();
    }
    if e[12] & 0x10 != 0 {
        x = x.to_lowercase();
    }
    if x.is_empty() { b } else { format!("{b}.{x}") }
}

// ------------------------------------------------------------------ exFAT

struct ExFat<'a> {
    src: &'a Source,
    vol: u64,
    cluster_size: u64,
    heap: u64,
    count: u32,
    fat_off: u64,
    root: u32,
}

impl<'a> ExFat<'a> {
    fn open(src: &'a Source, vol: &Volume) -> Result<Self> {
        let mut b = [0u8; 512];
        src.read_exact_at(vol.offset, &mut b)?;
        let bps_shift = b[108] as u32;
        let spc_shift = b[109] as u32;
        if !(9..=12).contains(&bps_shift) || bps_shift + spc_shift > 25 {
            bail!("invalid exFAT boot sector");
        }
        let bps = 1u64 << bps_shift;
        Ok(Self {
            src,
            vol: vol.offset,
            cluster_size: bps << spc_shift,
            heap: vol.offset + le32(&b, 88) as u64 * bps,
            count: le32(&b, 92),
            fat_off: vol.offset + le32(&b, 80) as u64 * bps,
            root: le32(&b, 96),
        })
    }

    fn coff(&self, c: u32) -> u64 {
        self.heap + (c as u64 - 2) * self.cluster_size
    }
    fn valid(&self, c: u32) -> bool {
        c >= 2 && c < self.count + 2
    }
    fn fat_entry(&self, c: u32) -> Result<u32> {
        let mut b = [0u8; 4];
        self.src
            .read_exact_at(self.fat_off + c as u64 * 4, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }
    fn chain(&self, start: u32, max: usize) -> Result<Vec<u32>> {
        let mut v = vec![];
        let mut seen = HashSet::new();
        let mut c = start;
        while self.valid(c) && seen.insert(c) && v.len() < max {
            v.push(c);
            c = self.fat_entry(c)?;
        }
        Ok(v)
    }
    fn contiguous(&self, first: u32, len: u64) -> Vec<u32> {
        let n = len.div_ceil(self.cluster_size).max(1) as u32;
        (first..first.saturating_add(n))
            .filter(|&c| self.valid(c))
            .collect()
    }

    fn analyze(self) -> Result<FsInfo> {
        let mut info = FsInfo {
            cluster_size: self.cluster_size,
            cluster_count: self.count,
            ..Default::default()
        };
        info.allocated.add(self.vol, self.heap);
        let mut bitmap: Option<Vec<u8>> = None;

        let root = self.chain(self.root, 1 << 16)?;
        let mut stack: Vec<(Vec<u32>, String, bool)> = vec![(root, String::new(), false)];
        let mut visited = HashSet::new();
        while let Some((clusters, path, in_deleted)) = stack.pop() {
            if clusters.is_empty() || !visited.insert(clusters[0]) {
                continue;
            }
            let mut data = vec![];
            for &c in &clusters {
                data.extend(
                    self.src
                        .read_vec(self.coff(c), self.cluster_size as usize)?,
                );
            }
            let mut i = 0;
            while i + 32 <= data.len() {
                let e = &data[i..i + 32];
                let t = e[0];
                if t == 0 {
                    break;
                }
                if t == 0x81 && bitmap.is_none() && !in_deleted {
                    let first = le32(e, 20);
                    let len = le64(e, 24);
                    let mut bm = vec![];
                    for c in self.contiguous(first, len) {
                        bm.extend(
                            self.src
                                .read_vec(self.coff(c), self.cluster_size as usize)?,
                        );
                    }
                    bm.truncate(len as usize);
                    bitmap = Some(bm);
                }
                // File directory entry: 0x85 in use, 0x05 deleted.
                if t & 0x7F == 0x05 {
                    let secondary = e[1] as usize;
                    let set_end = i + 32 * (secondary + 1);
                    if secondary < 2 || set_end > data.len() {
                        i += 32;
                        continue;
                    }
                    let deleted = t & 0x80 == 0 || in_deleted;
                    let attr = le16(e, 4);
                    let s = &data[i + 32..i + 64];
                    if s[0] & 0x7F != 0x40 {
                        i += 32;
                        continue;
                    }
                    // Deleted entries must be consistently deleted.
                    if (t & 0x80 == 0) != (s[0] & 0x80 == 0) {
                        i += 32;
                        continue;
                    }
                    let flags = s[1];
                    let name_len = s[3] as usize;
                    let first = le32(s, 20);
                    let size = le64(s, 24);
                    let mut units = vec![];
                    for k in 2..=secondary {
                        let ne = &data[i + 32 * k..i + 32 * (k + 1)];
                        if ne[0] & 0x7F != 0x41 {
                            break;
                        }
                        for j in 0..15 {
                            units.push(le16(ne, 2 + j * 2));
                        }
                    }
                    units.truncate(name_len);
                    let name = String::from_utf16_lossy(&units);
                    let full = format!("{path}/{name}");
                    let modified = dos_time((le32(e, 12) >> 16) as u16, le32(e, 12) as u16);
                    let created = dos_time((le32(e, 8) >> 16) as u16, le32(e, 8) as u16);
                    let no_fat_chain = flags & 2 != 0;
                    if attr & 0x10 != 0 {
                        if self.valid(first) && size > 0 {
                            let cl = if no_fat_chain || deleted {
                                self.contiguous(first, size)
                            } else {
                                self.chain(first, 1 << 16)?
                            };
                            stack.push((cl, full, deleted));
                        }
                    } else if !deleted {
                        info.live_files += 1;
                        let (cl, method) = if !self.valid(first) {
                            (vec![], "empty")
                        } else if no_fat_chain {
                            (self.contiguous(first, size), "nofatchain_contiguous")
                        } else {
                            (
                                self.chain(first, (size.div_ceil(self.cluster_size) + 1) as usize)?,
                                "fat_chain",
                            )
                        };
                        info.live.push(DeletedFile {
                            path: full.clone(),
                            size,
                            first_cluster: first,
                            extents: extents_of(&cl, size, self.cluster_size, |c| self.coff(c)),
                            method,
                            modified: modified.clone(),
                            created: created.clone(),
                        });
                    } else if size > 0 && self.valid(first) {
                        let (cl, method) = if no_fat_chain {
                            (self.contiguous(first, size), "nofatchain_contiguous")
                        } else {
                            // Whether the FAT chain survives deletion depends on the
                            // implementation; use it only if it is complete.
                            let ch =
                                self.chain(first, (size.div_ceil(self.cluster_size) + 1) as usize)?;
                            if ch.len() as u64 == size.div_ceil(self.cluster_size) {
                                (ch, "fat_chain")
                            } else {
                                (self.contiguous(first, size), "contiguous")
                            }
                        };
                        let mut extents: Vec<(u64, u64)> = vec![];
                        let mut left = size;
                        for c in cl {
                            let s0 = self.coff(c);
                            let l = left.min(self.cluster_size);
                            match extents.last_mut() {
                                Some(x) if x.1 == s0 => x.1 += l,
                                _ => extents.push((s0, s0 + l)),
                            }
                            left -= l;
                            if left == 0 {
                                break;
                            }
                        }
                        info.deleted.push(DeletedFile {
                            path: full,
                            size,
                            first_cluster: first,
                            extents,
                            method,
                            modified,
                            created,
                        });
                    }
                    i = set_end;
                    continue;
                }
                i += 32;
            }
        }
        if let Some(bm) = bitmap {
            for c in 0..self.count {
                if bm
                    .get((c / 8) as usize)
                    .is_some_and(|b| b >> (c % 8) & 1 == 1)
                {
                    let o = self.coff(c + 2);
                    info.allocated.add(o, o + self.cluster_size);
                }
            }
        }
        info.allocated.normalize();
        Ok(info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dos_time_decodes() {
        // 2024-05-17 13:45:30
        let date = ((2024 - 1980) << 9) | (5 << 5) | 17;
        let time = (13 << 11) | (45 << 5) | 15;
        assert_eq!(dos_time(date, time).as_deref(), Some("2024-05-17T13:45:30"));
    }
}
