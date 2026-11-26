//! NTFS metadata: MFT records of deleted and live files, and the cluster
//! allocation bitmap.
//!
//! Layout as publicly documented (Microsoft "NTFS Technical Reference",
//! the Linux-NTFS project's NTFS documentation, B. Carrier "File System
//! Forensic Analysis", ch. 11–13): boot sector BPB, MFT records with update
//! sequence fixups, resident/non-resident attributes and data runs.
//! Only reading is implemented.

use crate::fs::{DeletedFile, FsInfo};
use crate::io::{Source, Volume, le16, le32, le64};
use crate::ranges::Ranges;
use anyhow::{Result, bail};
use std::collections::HashMap;

const ROOT: u64 = 5;

pub struct Ntfs<'a> {
    src: &'a Source,
    vol: u64,
    cs: u64,
    rec_size: usize,
    clusters: u64,
    /// MFT location as (vcn, lcn, length) runs.
    mft_runs: Vec<(u64, u64, u64)>,
    mft_size: u64,
}

/// One data run: `lcn` None for a sparse run.
type Run = (u64, Option<u64>, u64);

fn decode_runs(b: &[u8], start_vcn: u64) -> Vec<Run> {
    let mut out = vec![];
    let (mut i, mut vcn, mut lcn) = (0usize, start_vcn, 0i64);
    while i < b.len() && b[i] != 0 {
        let h = b[i];
        let (ls, os) = ((h & 0x0F) as usize, (h >> 4) as usize);
        i += 1;
        if ls == 0 || ls > 8 || os > 8 || i + ls + os > b.len() {
            break;
        }
        let mut len = 0u64;
        for k in 0..ls {
            len |= (b[i + k] as u64) << (8 * k);
        }
        i += ls;
        let run_lcn = if os == 0 {
            None
        } else {
            let mut d = 0i64;
            for k in 0..os {
                d |= (b[i + k] as i64) << (8 * k);
            }
            // Sign-extend.
            let shift = 64 - 8 * os as u32;
            d = (d << shift) >> shift;
            lcn += d;
            i += os;
            (lcn >= 0).then_some(lcn as u64)
        };
        out.push((vcn, run_lcn, len));
        vcn += len;
    }
    out
}

/// FILETIME (100 ns since 1601, UTC) as ISO-8601 with `Z`.
fn filetime(ft: u64) -> Option<String> {
    if ft == 0 {
        return None;
    }
    let ms = (ft / 10_000) as i64 - 11_644_473_600_000;
    if !(0..4_102_444_800_000).contains(&ms) {
        return None;
    }
    Some(format!("{}Z", &crate::jdr::format_time(ms)[..19]))
}

#[derive(Default)]
struct Rec {
    in_use: bool,
    dir: bool,
    name: Option<(String, u8)>,
    parent: u64,
    modified: Option<String>,
    created: Option<String>,
    size: u64,
    /// (start vcn, runs) pieces of the unnamed $DATA.
    data: Vec<(u64, Vec<Run>)>,
    resident: bool,
    has_data: bool,
}

impl<'a> Ntfs<'a> {
    pub fn open(src: &'a Source, vol: &Volume) -> Result<Self> {
        let mut b = [0u8; 512];
        src.read_exact_at(vol.offset, &mut b)?;
        if &b[3..11] != b"NTFS    " {
            bail!("not NTFS");
        }
        let bps = le16(&b, 0x0B) as u64;
        let spc = b[0x0D] as u64;
        if !matches!(bps, 512 | 1024 | 2048 | 4096) || spc == 0 {
            bail!("bad NTFS boot sector");
        }
        let cs = bps * spc;
        let clusters = le64(&b, 0x28) / spc;
        let mft_lcn = le64(&b, 0x30);
        let c = b[0x40] as i8;
        let rec_size = if c > 0 {
            c as u64 * cs
        } else {
            1u64 << (-c as u32)
        } as usize;
        if !(512..=65536).contains(&rec_size) {
            bail!("bad MFT record size {rec_size}");
        }
        let mut n = Ntfs {
            src,
            vol: vol.offset,
            cs,
            rec_size,
            clusters,
            mft_runs: vec![(0, mft_lcn, u64::MAX / 2)],
            mft_size: rec_size as u64 * 16,
        };
        // Record 0 is $MFT itself: its $DATA gives the real MFT layout.
        let r0 = n
            .record(0)?
            .ok_or_else(|| anyhow::anyhow!("MFT record 0 unreadable"))?;
        let rec = n.parse(&r0);
        let mut runs = vec![];
        for (_, rs) in &rec.data {
            for &(vcn, lcn, len) in rs {
                if let Some(l) = lcn {
                    runs.push((vcn, l, len));
                }
            }
        }
        if runs.is_empty() {
            bail!("MFT has no data runs");
        }
        n.mft_runs = runs;
        n.mft_size = rec.size;
        Ok(n)
    }

    fn lcn_off(&self, lcn: u64) -> u64 {
        self.vol + lcn * self.cs
    }

    /// Reads MFT record `n` with fixups applied; None if not a FILE record.
    fn record(&self, n: u64) -> Result<Option<Vec<u8>>> {
        let byte = n * self.rec_size as u64;
        let vcn = byte / self.cs;
        let Some(&(v0, l0, _)) = self
            .mft_runs
            .iter()
            .find(|&&(v, _, len)| vcn >= v && vcn < v + len)
        else {
            return Ok(None);
        };
        let off = self.lcn_off(l0 + (vcn - v0)) + byte % self.cs;
        let mut b = vec![0u8; self.rec_size];
        // A record larger than a cluster could cross a run boundary; records
        // are normally <= cluster size.
        if self.src.read_at(off, &mut b)? < b.len() || &b[0..4] != b"FILE" {
            return Ok(None);
        }
        let usa = le16(&b, 4) as usize;
        let cnt = le16(&b, 6) as usize;
        if cnt < 2 || usa + 2 * cnt > b.len() || (cnt - 1) * 512 > b.len() {
            return Ok(None);
        }
        let check = [b[usa], b[usa + 1]];
        for k in 1..cnt {
            let end = k * 512 - 2;
            if b[end..end + 2] != check {
                return Ok(None); // torn write
            }
            b[end] = b[usa + 2 * k];
            b[end + 1] = b[usa + 2 * k + 1];
        }
        Ok(Some(b))
    }

    fn parse(&self, b: &[u8]) -> Rec {
        let mut r = Rec::default();
        let flags = le16(b, 0x16);
        r.in_use = flags & 1 != 0;
        r.dir = flags & 2 != 0;
        let mut off = le16(b, 0x14) as usize;
        while off + 16 <= b.len() {
            let ty = le32(b, off);
            if ty == 0xFFFF_FFFF {
                break;
            }
            let len = le32(b, off + 4) as usize;
            if len < 16 || off + len > b.len() {
                break;
            }
            let a = &b[off..off + len];
            let nonres = a[8] != 0;
            let name_len = a[9];
            match (ty, nonres) {
                (0x30, false) if len >= 24 => {
                    let co = le16(a, 0x14) as usize;
                    let cl = le32(a, 0x10) as usize;
                    if co + cl <= len && cl >= 66 {
                        let f = &a[co..co + cl];
                        let nl = f[64] as usize;
                        let ns = f[65];
                        if 66 + 2 * nl <= f.len() {
                            let u: Vec<u16> = (0..nl).map(|k| le16(f, 66 + 2 * k)).collect();
                            let name = String::from_utf16_lossy(&u);
                            // Prefer Win32/POSIX names over DOS 8.3 (namespace 2).
                            let better = match &r.name {
                                None => true,
                                Some((_, old)) => *old == 2 && ns != 2,
                            };
                            if better {
                                r.name = Some((name, ns));
                                r.parent = le64(f, 0) & 0xFFFF_FFFF_FFFF;
                                r.created = filetime(le64(f, 8));
                                r.modified = filetime(le64(f, 16));
                            }
                        }
                    }
                }
                (0x80, true) if name_len == 0 && len >= 0x40 => {
                    r.has_data = true;
                    let start_vcn = le64(a, 0x10);
                    if start_vcn == 0 {
                        r.size = le64(a, 0x30);
                    }
                    let ro = le16(a, 0x20) as usize;
                    if ro < len {
                        r.data.push((start_vcn, decode_runs(&a[ro..], start_vcn)));
                    }
                }
                (0x80, false) if name_len == 0 => {
                    r.has_data = true;
                    r.resident = true;
                    r.size = le32(a, 0x10) as u64;
                }
                _ => {}
            }
            off += len;
        }
        r
    }

    /// Absolute byte extents of `size` bytes along `runs`.
    fn extents(&self, runs: &[Run], size: u64) -> (Vec<(u64, u64)>, bool) {
        let mut ext: Vec<(u64, u64)> = vec![];
        let mut left = size;
        let mut sparse = false;
        for &(_, lcn, len) in runs {
            if left == 0 {
                break;
            }
            let bytes = (len * self.cs).min(left);
            match lcn {
                Some(l) if l + len <= self.clusters + 1 => {
                    let s = self.lcn_off(l);
                    match ext.last_mut() {
                        Some(x) if x.1 == s => x.1 += bytes,
                        _ => ext.push((s, s + bytes)),
                    }
                }
                _ => sparse = true,
            }
            left -= bytes;
        }
        (ext, sparse)
    }

    pub fn analyze(&self) -> Result<FsInfo> {
        let count = self.mft_size / self.rec_size as u64;
        let mut recs: HashMap<u64, Rec> = HashMap::new();
        // Extension records (base reference != 0) add data runs to their base.
        let mut ext_data: Vec<(u64, Vec<(u64, Vec<Run>)>, u64)> = vec![];
        for n in 0..count {
            let Some(b) = self.record(n)? else { continue };
            let base = le64(&b, 0x20) & 0xFFFF_FFFF_FFFF;
            let r = self.parse(&b);
            if base != 0 {
                ext_data.push((base, r.data, r.size));
            } else {
                recs.insert(n, r);
            }
        }
        for (base, data, size) in ext_data {
            if let Some(r) = recs.get_mut(&base) {
                r.has_data |= !data.is_empty();
                if r.size == 0 {
                    r.size = size;
                }
                r.data.extend(data);
            }
        }

        // Allocation bitmap ($Bitmap, record 6).
        let mut allocated = Ranges::new();
        if let Some(bm) = recs.get(&6) {
            let mut runs: Vec<Run> = bm.data.iter().flat_map(|d| d.1.clone()).collect();
            runs.sort_by_key(|r| r.0);
            let (ext, _) = self.extents(&runs, bm.size);
            let mut cl = 0u64;
            let mut start: Option<u64> = None;
            for (a, e) in ext {
                let bytes = self.src.read_vec(a, (e - a) as usize)?;
                for byte in bytes {
                    for bit in 0..8 {
                        let set = byte >> bit & 1 == 1;
                        match (set, start) {
                            (true, None) => start = Some(cl),
                            (false, Some(s)) => {
                                allocated.add(self.lcn_off(s), self.lcn_off(cl));
                                start = None;
                            }
                            _ => {}
                        }
                        cl += 1;
                        if cl >= self.clusters {
                            break;
                        }
                    }
                }
            }
            if let Some(s) = start {
                allocated.add(self.lcn_off(s), self.lcn_off(cl));
            }
        } else {
            // Without a bitmap treat the whole volume as in use.
            allocated.add(self.vol, self.lcn_off(self.clusters));
        }
        // Boot sector and the MFT itself.
        allocated.add(self.vol, self.vol + self.cs);
        for &(_, l, len) in &self.mft_runs {
            allocated.add(self.lcn_off(l), self.lcn_off(l + len));
        }
        allocated.normalize();

        let path_of = |n: u64| -> String {
            let mut parts = vec![];
            let mut cur = n;
            for _ in 0..64 {
                if cur == ROOT {
                    break;
                }
                match recs
                    .get(&cur)
                    .and_then(|r| r.name.as_ref().map(|x| (x.0.clone(), r.parent)))
                {
                    Some((name, p)) => {
                        parts.push(name);
                        if p == cur {
                            break;
                        }
                        cur = p;
                    }
                    None => {
                        parts.push(format!("$Orphan{cur}"));
                        break;
                    }
                }
            }
            parts.reverse();
            format!("/{}", parts.join("/"))
        };

        let mut info = FsInfo {
            allocated,
            cluster_size: self.cs,
            cluster_count: self.clusters.min(u32::MAX as u64) as u32,
            ..Default::default()
        };
        let mut keys: Vec<&u64> = recs.keys().collect();
        keys.sort();
        for &n in keys {
            let r = &recs[&n];
            // System files ($MFT ... $Extend) and directories are skipped.
            if n < 24 || r.dir || !r.has_data || r.name.is_none() {
                continue;
            }
            let mut runs: Vec<Run> = r.data.iter().flat_map(|d| d.1.clone()).collect();
            runs.sort_by_key(|x| x.0);
            let (extents, sparse) = self.extents(&runs, r.size);
            let first = runs.iter().find_map(|x| x.1).unwrap_or(0);
            let reused = !r.in_use
                && extents.iter().any(|&(a, b)| {
                    info.allocated.containing(a).is_some() || info.allocated.next_start(a) < b
                });
            let method = match (r.in_use, r.resident, sparse, reused) {
                (_, true, _, _) => "ntfs mft (resident data, not copied)",
                (true, _, _, _) => "ntfs mft",
                (false, _, true, _) => "ntfs mft (deleted, sparse runs)",
                (false, _, _, true) => "ntfs mft (deleted, some clusters now used by other files)",
                (false, _, _, false) => "ntfs mft (deleted)",
            };
            let f = DeletedFile {
                path: path_of(n),
                size: r.size,
                first_cluster: first.min(u32::MAX as u64) as u32,
                extents: if r.resident { vec![] } else { extents },
                method,
                modified: r.modified.clone(),
                created: r.created.clone(),
            };
            if r.in_use {
                info.live.push(f);
            } else {
                info.deleted.push(f);
            }
        }
        info.live_files = info.live.len();
        Ok(info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_runs() {
        // 0x21: 1-byte length, 2-byte offset. Second run negative offset.
        let b = [
            0x21, 0x10, 0x00, 0x10, 0x21, 0x08, 0x00, 0xF0, 0x01, 0x05, 0x00,
        ];
        let r = decode_runs(&b, 0);
        assert_eq!(r[0], (0, Some(0x1000), 0x10));
        assert_eq!(r[1], (0x10, Some(0x1000 - 0x1000), 0x08));
        assert_eq!(r[2], (0x18, None, 5));
    }

    #[test]
    fn filetime_epoch() {
        // 2026-11-10T00:00:00Z
        let ms: u64 = 1_794_268_800_000;
        let ft = (ms + 11_644_473_600_000) * 10_000;
        assert_eq!(filetime(ft).unwrap(), "2026-11-10T00:00:00Z");
    }
}
