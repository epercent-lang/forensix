//! JFS (IBM Journaled File System, "JFS2" on Linux) metadata: live files,
//! allocated space and deleted files.
//!
//! Layout as defined in the Linux kernel sources fs/jfs/jfs_superblock.h,
//! jfs_dinode.h, jfs_imap.h, jfs_xtree.h, jfs_dtree.h and jfs_dmap.h: super
//! block at 0x8000, aggregate inode table at 0xB000, fileset inode map
//! (aggregate inode 16) made of IAG pages that list inode extents, block
//! allocation map (aggregate inode 2) made of dmap pages. Only reading is
//! implemented.
//!
//! Deleting a file frees its inode and blocks in the maps but leaves the
//! inode on disk with its size and extent tree, and link count 0. Such
//! inodes are used when all of their blocks are still free. The directory
//! slot keeps the name but loses the low half of the inode number, so a
//! name is attached only when the order of entries in the directory leaves
//! one possible inode; otherwise the file is named after its inode number.
//! Inode extents that were released as a whole are not searched.

use crate::fs::{DeletedFile, FsInfo};
use crate::io::{Source, Volume, le16, le32, le64};
use crate::ranges::Ranges;
use crate::xfs::unix_time;
use anyhow::{Result, bail};
use std::collections::{HashMap, HashSet};

const PAGE: u64 = 4096;
const INODE: usize = 512;
/// Offset of the extent tree / directory tree root inside an inode.
const ROOT: usize = 224;

pub fn is_jfs(sb: &[u8]) -> bool {
    sb.len() >= 64
        && &sb[0..4] == b"JFS1"
        && le32(sb, 16).is_power_of_two()
        && (512..=4096).contains(&le32(sb, 16))
}

/// (length, address) of a pxd_t, both in file system blocks.
fn pxd(b: &[u8], o: usize) -> (u64, u64) {
    let la = le32(b, o) as u64;
    (la & 0xFF_FFFF, (la >> 24) << 32 | le32(b, o + 4) as u64)
}

struct Inode {
    mode: u32,
    nlink: u32,
    size: u64,
    mtime: Option<String>,
    otime: Option<String>,
    raw: Vec<u8>,
}

/// One directory slot holding the start of an entry.
struct Entry {
    ino: u32,
    name: String,
    index: u32,
    live: bool,
}

pub struct Jfs<'a> {
    src: &'a Source,
    vol: u64,
    bs: u64,
    blocks: u64,
    /// Directory entries carry an index field (shorter name part).
    dir_index: bool,
}

impl<'a> Jfs<'a> {
    pub fn open(src: &'a Source, vol: &Volume) -> Result<Self> {
        let sb = src.read_vec(vol.offset + 0x8000, 512)?;
        if !is_jfs(&sb) {
            bail!("not a JFS volume");
        }
        let bs = le32(&sb, 16) as u64;
        let pbs = (le32(&sb, 24) as u64).max(1);
        Ok(Jfs {
            src,
            vol: vol.offset,
            bs,
            blocks: le64(&sb, 8) * pbs / bs,
            dir_index: le32(&sb, 36) & 0x0020_0000 != 0,
        })
    }

    fn page(&self, block: u64) -> Result<Vec<u8>> {
        if block == 0 || block >= self.blocks {
            bail!("block outside the volume");
        }
        self.src
            .read_vec(self.vol + block * self.bs, PAGE as usize)
    }

    /// Extents (file block, length, disk block) of an extent tree whose
    /// root header is at `base` of `b`.
    fn xtree(&self, b: &[u8], base: usize, depth: u32, out: &mut Vec<(u64, u64, u64)>) -> bool {
        let flag = b[base + 16];
        let n = le16(b, base + 18) as usize;
        if n < 2 || base + 16 * n > b.len() || depth > 8 || out.len() > 1 << 22 {
            return n == 2 && flag & 2 != 0;
        }
        for i in 2..n {
            let e = base + 16 * i;
            let off = (b[e + 3] as u64) << 32 | le32(b, e + 4) as u64;
            let (len, addr) = pxd(b, e + 8);
            if flag & 2 != 0 {
                if len == 0 || addr == 0 || addr + len > self.blocks {
                    return false;
                }
                out.push((off, len, addr));
            } else {
                let Ok(p) = self.page(addr) else { return false };
                if !self.xtree(&p, 0, depth + 1, out) {
                    return false;
                }
            }
        }
        true
    }

    fn inode_extents(&self, raw: &[u8]) -> Option<Vec<(u64, u64, u64)>> {
        let mut out = vec![];
        self.xtree(raw, ROOT, 0, &mut out).then_some(out)
    }

    /// Absolute byte extents of the first `size` bytes; `None` with holes.
    fn byte_extents(&self, ext: &[(u64, u64, u64)], size: u64) -> Option<Vec<(u64, u64)>> {
        let mut out: Vec<(u64, u64)> = vec![];
        let (mut pos, mut left) = (0u64, size);
        for &(off, len, addr) in ext {
            if left == 0 {
                break;
            }
            if off != pos {
                return None;
            }
            let l = left.min(len * self.bs);
            let a = self.vol + addr * self.bs;
            match out.last_mut() {
                Some(x) if x.1 == a => x.1 += l,
                _ => out.push((a, a + l)),
            }
            pos += len;
            left -= l;
        }
        (left == 0).then_some(out)
    }

    /// Whole contents of a metadata file (inode map, block map).
    fn read_file(&self, raw: &[u8], limit: u64) -> Result<Vec<u8>> {
        let Some(ext) = self.inode_extents(raw) else {
            bail!("unreadable extent tree");
        };
        let mut out = vec![];
        for (off, len, addr) in ext {
            if off * self.bs != out.len() as u64 || out.len() as u64 > limit {
                break;
            }
            out.extend(
                self.src
                    .read_vec(self.vol + addr * self.bs, (len * self.bs) as usize)?,
            );
        }
        Ok(out)
    }

    fn parse_inode(raw: &[u8]) -> Inode {
        let t = |o: usize| unix_time(le32(raw, o) as i64);
        Inode {
            mode: le32(raw, 52),
            nlink: le32(raw, 40),
            size: le64(raw, 24),
            mtime: t(72),
            otime: t(80),
            raw: raw.to_vec(),
        }
    }

    /// Entries of one directory node (`b` = the 32-byte slots, slot 0 being
    /// the header at `base`), and child pages of an internal node.
    fn dir_node(&self, b: &[u8], base: usize, root: bool, out: &mut Vec<Entry>, kids: &mut Vec<u64>) {
        let slots = if root { 9 } else { 128 };
        if base + slots * 32 > b.len() {
            return;
        }
        let flag = b[base + 16];
        let n = b[base + 17] as usize;
        let (stbl, first) = if root {
            (base + 24, 1)
        } else {
            let si = b[base + 21] as usize;
            if si == 0 || si >= slots {
                return;
            }
            // The sort table takes whole slots after the header.
            (base + si * 32, si + slots.div_ceil(32))
        };
        let head = if self.dir_index { 11 } else { 13 };
        let name_at = |s: usize, internal: bool| -> Option<String> {
            let e = base + s * 32;
            let (len, at, cap) = if internal {
                (b[e + 9] as usize, e + 10, 11)
            } else {
                (b[e + 5] as usize, e + 6, head)
            };
            if len == 0 {
                return None;
            }
            let mut units: Vec<u16> = (0..len.min(cap)).map(|k| le16(b, at + 2 * k)).collect();
            let mut next = b[e + if internal { 8 } else { 4 }] as i8;
            let mut guard = 0;
            while units.len() < len && next > 0 && (next as usize) < slots && guard < 20 {
                let c = base + next as usize * 32;
                units.extend((0..(len - units.len()).min(15)).map(|k| le16(b, c + 2 + 2 * k)));
                next = b[c] as i8;
                guard += 1;
            }
            (units.len() == len && units.iter().all(|&u| u != 0 && u != b'/' as u16))
                .then(|| String::from_utf16_lossy(&units))
        };
        let mut used: HashSet<usize> = HashSet::new();
        for k in 0..n.min(slots) {
            let s = b[stbl + k] as usize;
            if s == 0 || s >= slots {
                continue;
            }
            used.insert(s);
            let e = base + s * 32;
            if flag & 4 != 0 {
                kids.push(pxd(b, e).1);
            } else if let Some(name) = name_at(s, false) {
                out.push(Entry {
                    ino: le32(b, e),
                    name,
                    index: if self.dir_index { le32(b, e + 28) } else { 0 },
                    live: true,
                });
            }
        }
        if flag & 4 != 0 {
            return;
        }
        // Freed slots: the first two bytes were replaced by free list links.
        for s in first.max(1)..slots {
            if used.contains(&s) {
                continue;
            }
            let e = base + s * 32;
            let len = b[e + 5] as usize;
            if len == 0 || len > head || b[e + 4] != 0xFF {
                continue;
            }
            if let Some(name) = name_at(s, false)
                && name.chars().all(|c| !c.is_control())
            {
                out.push(Entry {
                    ino: le32(b, e) & 0xFFFF_0000,
                    name,
                    index: if self.dir_index { le32(b, e + 28) } else { 0 },
                    live: false,
                });
            }
        }
    }

    fn dir_entries(&self, raw: &[u8]) -> Vec<Entry> {
        let (mut out, mut kids) = (vec![], vec![]);
        self.dir_node(raw, ROOT, true, &mut out, &mut kids);
        let mut seen = HashSet::new();
        while let Some(k) = kids.pop() {
            if !seen.insert(k) || seen.len() > 1 << 20 {
                continue;
            }
            if let Ok(p) = self.page(k) {
                self.dir_node(&p, 0, false, &mut out, &mut kids);
            }
        }
        out
    }

    fn allocation(&self) -> Result<Ranges> {
        let mut r = Ranges::new();
        let ino = self.src.read_vec(self.vol + 0xB000 + 2 * INODE as u64, INODE)?;
        let Some(ext) = self.inode_extents(&ino) else {
            bail!("block map unreadable");
        };
        // Page of the map file -> disk block.
        let page_block = |p: u64| {
            let fb = p * PAGE / self.bs;
            ext.iter()
                .find(|e| e.0 <= fb && fb < e.0 + e.1)
                .map(|e| e.2 + (fb - e.0))
        };
        let mut run: Option<u64> = None;
        let mut first = 0u64;
        while first < self.blocks {
            let p = (first >> 13) + (first >> 23) + (first >> 33) + 4;
            let dmap = page_block(p).and_then(|b| self.page(b).ok());
            for w in 0..256u64 {
                // Unreadable map pages count as allocated.
                let v = dmap.as_ref().map_or(u32::MAX, |d| le32(d, 3072 + 4 * w as usize));
                for bit in 0..32u64 {
                    let blk = first + w * 32 + bit;
                    if blk >= self.blocks {
                        break;
                    }
                    let used = v & (0x8000_0000 >> bit) != 0;
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
            first += 8192;
        }
        if let Some(s) = run {
            r.add(self.vol + s * self.bs, self.vol + self.blocks * self.bs);
        }
        r.normalize();
        Ok(r)
    }

    pub fn analyze(&self) -> Result<FsInfo> {
        let allocated = self.allocation()?;
        let mut info = FsInfo {
            cluster_size: self.bs,
            cluster_count: self.blocks.min(u32::MAX as u64) as u32,
            ..Default::default()
        };
        // Every inode of the fileset, through the inode map's IAG pages.
        let fileset = self.src.read_vec(self.vol + 0xB000 + 16 * INODE as u64, INODE)?;
        let imap = self.read_file(&fileset, 1 << 30)?;
        let mut inodes: HashMap<u32, Inode> = HashMap::new();
        for (n, iag) in imap.chunks_exact(PAGE as usize).skip(1).enumerate() {
            if le32(iag, 8) as usize != n {
                continue;
            }
            for k in 0..128usize {
                let (len, addr) = pxd(iag, 3072 + 8 * k);
                if len == 0 || addr == 0 || addr + len > self.blocks {
                    continue;
                }
                let Ok(buf) = self.src.read_vec(self.vol + addr * self.bs, 32 * INODE) else {
                    continue;
                };
                for (i, raw) in buf.chunks_exact(INODE).enumerate() {
                    let num = (n * 4096 + k * 32 + i) as u32;
                    if le32(raw, 8) == num && le32(raw, 52) != 0 {
                        inodes.insert(num, Self::parse_inode(raw));
                    }
                }
            }
        }

        // Directory tree from the root (inode 2).
        let mut live: HashSet<u32> = HashSet::new();
        let mut names: HashMap<u32, String> = HashMap::new();
        let mut seen: HashSet<u32> = HashSet::new();
        let mut stack = vec![(2u32, String::new())];
        while let Some((dino, dpath)) = stack.pop() {
            if !seen.insert(dino) {
                continue;
            }
            let Some(d) = inodes.get(&dino) else { continue };
            if d.mode & 0xF000 != 0x4000 {
                continue;
            }
            let mut entries = self.dir_entries(&d.raw);
            entries.sort_by_key(|e| e.index);
            for (k, e) in entries.iter().enumerate() {
                let path = format!("{dpath}/{}", e.name);
                if e.live {
                    match inodes.get(&e.ino).map(|i| i.mode & 0xF000) {
                        Some(0x4000) => stack.push((e.ino, path)),
                        Some(0x8000) => {
                            live.insert(e.ino);
                            names.insert(e.ino, path);
                        }
                        _ => {}
                    }
                    continue;
                }
                if !self.dir_index {
                    continue;
                }
                // Removed entries between the same two live neighbours (in
                // creation order) against the unlinked inodes numbered
                // between those neighbours: a name is given only when the
                // counts agree.
                let lo = entries[..k].iter().rev().find(|x| x.live).map(|x| x.ino);
                let hi = entries[k + 1..].iter().find(|x| x.live).map(|x| x.ino);
                let from = k - entries[..k].iter().rev().take_while(|x| !x.live).count();
                let group: Vec<&Entry> =
                    entries[from..].iter().take_while(|x| !x.live).collect();
                let mut cand: Vec<u32> = inodes
                    .iter()
                    .filter(|(n, i)| {
                        i.nlink == 0
                            && **n & 0xFFFF_0000 == e.ino
                            && lo.is_none_or(|l| **n > l)
                            && hi.is_none_or(|h| **n < h)
                            && **n > dino
                    })
                    .map(|(n, _)| *n)
                    .collect();
                cand.sort();
                if (lo.is_some() || hi.is_some()) && cand.len() == group.len() {
                    names.entry(cand[k - from]).or_insert(path);
                }
            }
        }

        for (&num, i) in &inodes {
            if i.mode & 0xF000 != 0x8000 || i.size == 0 || num < 4 {
                continue;
            }
            let Some(extents) = self
                .inode_extents(&i.raw)
                .and_then(|x| self.byte_extents(&x, i.size))
            else {
                continue;
            };
            let rec = |path: String, method: &'static str| DeletedFile {
                path,
                size: i.size,
                first_cluster: 0,
                extents: extents.clone(),
                method,
                modified: i.mtime.clone(),
                created: i.otime.clone(),
            };
            if live.contains(&num) {
                info.live.push(rec(names[&num].clone(), "inode extents"));
            } else if i.nlink == 0
                && extents
                    .iter()
                    .all(|&(a, b)| allocated.gaps(a, b) == vec![(a, b)])
            {
                match names.get(&num) {
                    Some(p) => info.deleted.push(rec(
                        p.clone(),
                        "JFS: unlinked inode with its extents (name matched by directory order); blocks still free",
                    )),
                    None => info.deleted.push(rec(
                        format!("/jfs_inode_{num}"),
                        "JFS: unlinked inode with its extents; blocks still free",
                    )),
                }
            }
        }
        info.live.sort_by(|a, b| a.path.cmp(&b.path));
        info.deleted.sort_by(|a, b| a.path.cmp(&b.path));
        info.live_files = info.live.len();
        info.allocated = allocated;
        Ok(info)
    }
}
