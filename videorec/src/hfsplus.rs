//! HFS+ / HFSX metadata: live files, allocated space, and deleted files
//! whose catalog records are still readable.
//!
//! Layout as documented in Apple Technical Note TN1150 "HFS Plus Volume
//! Format": volume header at byte 1024, allocation file (bitmap), catalog
//! and extents overflow B-trees, optional journal. Only reading is
//! implemented. HFS wrappers, Apple Partition Map, hard links and
//! compressed files (decmpfs) are not handled.
//!
//! Removing a file deletes its record from a catalog leaf node. The bytes
//! of a record often stay in the node's unused space (always when it was
//! the last record of the node), in nodes that were emptied and released,
//! and in older copies of catalog nodes inside the journal. Those places
//! are searched for file records of files the catalog no longer has; one is
//! used only when all of its blocks are free in the allocation file.

use crate::fs::{DeletedFile, FsInfo};
use crate::io::{Source, Volume, be32, be64};
use crate::ranges::Ranges;
use crate::xfs::unix_time;
use anyhow::{Result, bail};
use std::collections::{HashMap, HashSet};

fn be16(b: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([b[o], b[o + 1]])
}

pub fn is_hfsplus(vh: &[u8]) -> bool {
    vh.len() >= 512
        && ((&vh[0..2] == b"H+" && be16(vh, 2) == 4) || (&vh[0..2] == b"HX" && be16(vh, 2) == 5))
        && be32(vh, 40).is_power_of_two()
        && be32(vh, 40) >= 512
}

/// Seconds since 1904-01-01 (UTC) as ISO-8601.
fn mac_time(v: u32) -> Option<String> {
    if v == 0 {
        return None;
    }
    unix_time(v as i64 - 2_082_844_800)
}

/// A fork: (start block, block count) runs in file order, and its size.
#[derive(Clone, Default)]
struct Fork {
    size: u64,
    blocks: u64,
    ext: Vec<(u64, u64)>,
}

fn fork_at(b: &[u8], o: usize) -> Fork {
    Fork {
        size: be64(b, o),
        blocks: be32(b, o + 12) as u64,
        ext: (0..8)
            .map(|k| {
                (
                    be32(b, o + 16 + k * 8) as u64,
                    be32(b, o + 20 + k * 8) as u64,
                )
            })
            .filter(|e| e.1 > 0)
            .collect(),
    }
}

struct FileRec {
    id: u32,
    parent: u32,
    name: String,
    fork: Fork,
    modified: Option<String>,
    created: Option<String>,
}

pub struct HfsPlus<'a> {
    src: &'a Source,
    vol: u64,
    bs: u64,
    total: u64,
    alloc: Fork,
    extents: Fork,
    catalog: Fork,
    journal_info: u64,
}

impl<'a> HfsPlus<'a> {
    pub fn open(src: &'a Source, vol: &Volume) -> Result<Self> {
        let vh = src.read_vec(vol.offset + 1024, 512)?;
        if !is_hfsplus(&vh) {
            bail!("not an HFS+ volume");
        }
        let journaled = be32(&vh, 4) & (1 << 13) != 0;
        Ok(HfsPlus {
            src,
            vol: vol.offset,
            bs: be32(&vh, 40) as u64,
            total: be32(&vh, 44) as u64,
            alloc: fork_at(&vh, 112),
            extents: fork_at(&vh, 192),
            catalog: fork_at(&vh, 272),
            journal_info: if journaled { be32(&vh, 12) as u64 } else { 0 },
        })
    }

    /// Reads `len` bytes of a fork at `off`; short when the extents end.
    fn read_fork(&self, f: &Fork, off: u64, len: usize) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(len);
        let (mut pos, end) = (0u64, off + len as u64);
        for &(s, n) in &f.ext {
            let (a, b) = (pos, pos + n * self.bs);
            pos = b;
            let (lo, hi) = (a.max(off), b.min(end));
            if lo < hi {
                if s + n > self.total {
                    bail!("extent beyond the volume");
                }
                out.extend(
                    self.src
                        .read_vec(self.vol + s * self.bs + (lo - a), (hi - lo) as usize)?,
                );
            }
        }
        Ok(out)
    }

    /// Node size and node count of a B-tree file.
    fn tree(&self, f: &Fork) -> Result<(usize, u64, u32)> {
        let h = self.read_fork(f, 0, 512)?;
        if h.len() < 512 || h[8] != 1 {
            bail!("B-tree header node not found");
        }
        let ns = be16(&h, 14 + 18) as usize;
        if !ns.is_power_of_two() || !(512..=32768).contains(&ns) {
            bail!("unsupported B-tree node size");
        }
        let have = f.ext.iter().map(|e| e.1).sum::<u64>() * self.bs / ns as u64;
        Ok((ns, (be32(&h, 14 + 22) as u64).min(have), be32(&h, 14 + 10)))
    }

    /// Records of a leaf node as byte ranges; `None` when the node is not a
    /// well-formed leaf.
    fn leaf_records(node: &[u8]) -> Option<Vec<(usize, usize)>> {
        let ns = node.len();
        if node[8] != 0xFF {
            return None;
        }
        let n = be16(node, 10) as usize;
        if 14 + 2 * (n + 1) > ns {
            return None;
        }
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let a = be16(node, ns - 2 * (i + 1)) as usize;
            let b = be16(node, ns - 2 * (i + 2)) as usize;
            if a < 14 || b <= a || b > ns - 2 * (n + 1) {
                return None;
            }
            out.push((a, b));
        }
        Some(out)
    }

    /// Extents beyond a fork's first eight, from the extents overflow file:
    /// (file id, fork type) -> runs sorted by their first file block.
    fn overflow(&self) -> HashMap<(u32, u8), Vec<(u32, Vec<(u64, u64)>)>> {
        let mut map: HashMap<(u32, u8), Vec<(u32, Vec<(u64, u64)>)>> = HashMap::new();
        let Ok((ns, nodes, _)) = self.tree(&self.extents) else {
            return map;
        };
        for n in 0..nodes.min(1 << 22) {
            let Ok(node) = self.read_fork(&self.extents, n * ns as u64, ns) else {
                break;
            };
            if node.len() < ns {
                break;
            }
            for (a, b) in Self::leaf_records(&node).unwrap_or_default() {
                if b - a < 12 + 64 || be16(&node, a) != 10 {
                    continue;
                }
                let key = (be32(&node, a + 4), node[a + 2]);
                let runs = (0..8)
                    .map(|k| {
                        (
                            be32(&node, a + 12 + k * 8) as u64,
                            be32(&node, a + 16 + k * 8) as u64,
                        )
                    })
                    .filter(|e| e.1 > 0)
                    .collect();
                map.entry(key)
                    .or_default()
                    .push((be32(&node, a + 8), runs));
            }
        }
        for v in map.values_mut() {
            v.sort_by_key(|e| e.0);
        }
        map
    }

    /// Absolute byte extents of the first `size` bytes of a fork.
    fn byte_extents(&self, runs: &[(u64, u64)], size: u64) -> Option<Vec<(u64, u64)>> {
        let mut out: Vec<(u64, u64)> = vec![];
        let mut left = size;
        for &(s, n) in runs {
            if left == 0 {
                break;
            }
            if n == 0 || s + n > self.total {
                return None;
            }
            let l = left.min(n * self.bs);
            let a = self.vol + s * self.bs;
            match out.last_mut() {
                Some(x) if x.1 == a => x.1 += l,
                _ => out.push((a, a + l)),
            }
            left -= l;
        }
        (left == 0).then_some(out)
    }

    /// A catalog file record whose key starts at `o`, if the bytes there
    /// look like one.
    fn file_record(&self, b: &[u8], o: usize) -> Option<FileRec> {
        if o + 8 > b.len() {
            return None;
        }
        let kl = be16(b, o) as usize;
        let nl = be16(b, o + 6) as usize;
        if !(1..=255).contains(&nl) || kl != 6 + 2 * nl {
            return None;
        }
        let r = o + 2 + kl;
        if r + 248 > b.len() || be16(b, r) != 2 {
            return None;
        }
        let (parent, id) = (be32(b, o + 2), be32(b, r + 8));
        if parent == 0 || id < 16 {
            return None;
        }
        let units: Vec<u16> = (0..nl).map(|k| be16(b, o + 8 + 2 * k)).collect();
        if units.iter().any(|&u| u == 0 && nl > 1) && parent != 2 {
            // A NUL inside a name only occurs in the private metadata folder.
            return None;
        }
        let name = String::from_utf16(&units).ok()?.replace(['/', '\0'], "_");
        let fork = fork_at(b, r + 88);
        if fork.blocks > self.total || fork.size > fork.blocks.saturating_mul(self.bs) {
            return None;
        }
        Some(FileRec {
            id,
            parent,
            name,
            fork,
            modified: mac_time(be32(b, r + 16)),
            created: mac_time(be32(b, r + 12)),
        })
    }

    /// The data fork of a file record whose beginning was overwritten: the
    /// file mode field would be at `m`, the fork 46 bytes later. Accepted
    /// only when the extents add up exactly.
    fn bare_fork(&self, b: &[u8], m: usize) -> Option<Fork> {
        if m + 46 + 80 > b.len() || b[m] & 0xF0 != 0x80 {
            return None;
        }
        let f = fork_at(b, m + 46);
        let need = f.size.div_ceil(self.bs);
        if f.size == 0
            || f.blocks < need
            || f.blocks > need + (16 << 20) / self.bs
            || f.ext.iter().map(|e| e.1).sum::<u64>() != f.blocks
            || f.ext.iter().any(|e| e.0 == 0 || e.0 + e.1 > self.total)
        {
            return None;
        }
        // Unused extent slots are zero and come last.
        let used = f.ext.len();
        (used..8)
            .all(|k| be64(b, m + 46 + 16 + k * 8) == 0)
            .then_some(f)
    }

    fn allocation(&self) -> Result<Ranges> {
        let mut r = Ranges::new();
        let bytes = self.total.div_ceil(8);
        let (mut off, mut run): (u64, Option<u64>) = (0, None);
        while off < bytes {
            let n = (bytes - off).min(1 << 20) as usize;
            let buf = self.read_fork(&self.alloc, off, n)?;
            if buf.is_empty() {
                break;
            }
            for (i, &v) in buf.iter().enumerate() {
                if v == 0 && run.is_none() {
                    continue;
                }
                for bit in 0..8u64 {
                    let blk = (off + i as u64) * 8 + bit;
                    if blk >= self.total {
                        break;
                    }
                    let used = v & (0x80 >> bit) != 0;
                    match (used, run) {
                        (true, None) => run = Some(blk),
                        (false, Some(s)) => {
                            r.add(self.vol + s * self.bs, self.vol + blk * self.bs);
                            run = None;
                        }
                        _ => {}
                    }
                }
            }
            off += buf.len() as u64;
        }
        if let Some(s) = run {
            r.add(self.vol + s * self.bs, self.vol + self.total * self.bs);
        }
        // Boot blocks, volume header and the alternate header at the end.
        r.add(self.vol, self.vol + 1536.max(self.bs));
        r.normalize();
        Ok(r)
    }

    /// The journal's bytes (at most 256 MiB), when it lies inside the volume.
    fn journal(&self) -> Vec<u8> {
        if self.journal_info == 0 || self.journal_info >= self.total {
            return vec![];
        }
        let Ok(jib) = self.src.read_vec(self.vol + self.journal_info * self.bs, 52) else {
            return vec![];
        };
        // Flags bit 0: the journal is on this volume; then offset and size.
        let (off, size) = (be64(&jib, 36), be64(&jib, 44));
        if be32(&jib, 0) & 1 == 0 || size == 0 || off + size > self.total * self.bs {
            return vec![];
        }
        self.src
            .read_vec(self.vol + off, size.min(256 << 20) as usize)
            .unwrap_or_default()
    }

    pub fn analyze(&self) -> Result<FsInfo> {
        let allocated = self.allocation()?;
        let overflow = self.overflow();
        let (ns, nodes, _) = self.tree(&self.catalog)?;
        let mut info = FsInfo {
            cluster_size: self.bs,
            cluster_count: self.total.min(u32::MAX as u64) as u32,
            ..Default::default()
        };
        let mut folders: HashMap<u32, (u32, String)> = HashMap::new();
        let mut files: Vec<FileRec> = vec![];
        let mut live: HashSet<u32> = HashSet::new();
        let mut old: Vec<FileRec> = vec![];
        let mut bare: Vec<Fork> = vec![];
        for n in 0..nodes {
            let Ok(node) = self.read_fork(&self.catalog, n * ns as u64, ns) else {
                continue;
            };
            if node.len() < ns {
                break;
            }
            let recs = Self::leaf_records(&node).unwrap_or_default();
            let mut used_end = 14;
            for &(a, b) in &recs {
                used_end = used_end.max(b);
                let kl = be16(&node, a) as usize;
                let r = a + 2 + kl + (kl & 1);
                if kl < 6 || r + 2 > b {
                    continue;
                }
                match be16(&node, r) {
                    1 if r + 12 <= b => {
                        let nl = (be16(&node, a + 6) as usize).min((kl - 6) / 2);
                        let units: Vec<u16> =
                            (0..nl).map(|k| be16(&node, a + 8 + 2 * k)).collect();
                        let name = String::from_utf16_lossy(&units).replace(['/', '\0'], "_");
                        folders.insert(be32(&node, r + 8), (be32(&node, a + 2), name));
                    }
                    2 => {
                        if let Some(f) = self.file_record(&node[..b], a) {
                            live.insert(f.id);
                            files.push(f);
                        }
                    }
                    _ => {}
                }
            }
            // Unused space of the node (the whole node when it is not a
            // leaf in use): older records.
            let from = if recs.is_empty() { 14 } else { used_end };
            for o in (from & !1..ns.saturating_sub(250)).step_by(2) {
                if let Some(f) = self.file_record(&node, o) {
                    old.push(f);
                }
            }
            for m in (from & !1..ns.saturating_sub(126)).step_by(2) {
                if let Some(f) = self.bare_fork(&node, m) {
                    bare.push(f);
                }
            }
        }
        let journal = self.journal();
        for o in (0..journal.len().saturating_sub(250)).step_by(2) {
            if be16(&journal, o + 6) != 0
                && let Some(f) = self.file_record(&journal, o)
            {
                old.push(f);
            }
            if journal[o] & 0xF0 == 0x80
                && let Some(f) = self.bare_fork(&journal, o)
            {
                bare.push(f);
            }
        }

        let path_of = |parent: u32, name: &str| {
            let mut parts = vec![name.to_string()];
            let (mut p, mut guard) = (parent, 0);
            while p != 2 && guard < 256 {
                match folders.get(&p) {
                    Some((pp, n)) => {
                        parts.push(n.clone());
                        p = *pp;
                    }
                    None => {
                        parts.push(format!("hfs_folder_{p}"));
                        break;
                    }
                }
                guard += 1;
            }
            parts.reverse();
            format!("/{}", parts.join("/"))
        };
        let runs_of = |f: &FileRec| {
            let mut runs = f.fork.ext.clone();
            if let Some(more) = overflow.get(&(f.id, 0)) {
                for (start, r) in more {
                    if *start as u64 == runs.iter().map(|e| e.1).sum::<u64>() {
                        runs.extend(r.iter().copied());
                    }
                }
            }
            runs
        };
        for f in &files {
            if f.fork.size == 0 {
                continue;
            }
            if let Some(extents) = self.byte_extents(&runs_of(f), f.fork.size) {
                info.live.push(DeletedFile {
                    path: path_of(f.parent, &f.name),
                    size: f.fork.size,
                    first_cluster: 0,
                    extents,
                    method: "catalog record",
                    modified: f.modified.clone(),
                    created: f.created.clone(),
                });
            }
        }
        info.live_files = info.live.len();

        let mut seen: HashSet<u32> = HashSet::new();
        for f in &old {
            if f.fork.size == 0 || live.contains(&f.id) || seen.contains(&f.id) {
                continue;
            }
            let Some(extents) = self.byte_extents(&runs_of(f), f.fork.size) else {
                continue;
            };
            if extents
                .iter()
                .any(|&(a, b)| allocated.gaps(a, b) != vec![(a, b)])
            {
                continue;
            }
            seen.insert(f.id);
            info.deleted.push(DeletedFile {
                path: path_of(f.parent, &f.name),
                size: f.fork.size,
                first_cluster: 0,
                extents,
                method: "HFS+ catalog: record left in unused node space or in the journal; blocks still free",
                modified: f.modified.clone(),
                created: f.created.clone(),
            });
        }
        // Records without their beginning: no name, but size and extents.
        let mut starts: HashSet<u64> = info
            .deleted
            .iter()
            .chain(&info.live)
            .filter_map(|d| d.extents.first().map(|e| e.0))
            .collect();
        for f in &bare {
            let Some(extents) = self.byte_extents(&f.ext, f.size) else {
                continue;
            };
            if !starts.insert(extents[0].0)
                || extents
                    .iter()
                    .any(|&(a, b)| allocated.gaps(a, b) != vec![(a, b)])
            {
                continue;
            }
            let head = self.src.read_vec(extents[0].0, 12).unwrap_or_default();
            let ext = match head.get(..12) {
                Some(h) if &h[4..8] == b"ftyp" => "mp4",
                Some(h) if &h[..4] == b"RIFF" => "avi",
                _ => "bin",
            };
            info.deleted.push(DeletedFile {
                path: format!("/hfs_deleted_block_{}.{ext}", f.ext[0].0),
                size: f.size,
                first_cluster: 0,
                extents,
                method: "HFS+ catalog: end of a removed record (size and extents, name lost); blocks still free",
                modified: None,
                created: None,
            });
        }
        info.deleted.sort_by(|a, b| a.path.cmp(&b.path));
        info.allocated = allocated;
        Ok(info)
    }
}
