//! ext2/ext3/ext4 metadata: live files, the block allocation bitmap, and
//! deleted files whose block list can still be found.
//!
//! Layout as documented in the Linux kernel documentation
//! (Documentation/filesystems/ext4: super block, group descriptors, inodes,
//! extent tree, directory entries, and the jbd2 journal). Only reading is
//! implemented.
//!
//! Deleting a file on ext4 clears the extent list in its inode, so the inode
//! on disk no longer says where the data was. Older copies of the inode
//! table block usually remain in the journal; a copy made while the file
//! still existed gives the extents back. Names of deleted files come from
//! directory entries left in the unused tail of a neighbouring entry.

use crate::fs::{DeletedFile, FsInfo};
use crate::io::{Source, Volume, le16, le32};
use crate::ranges::Ranges;
use anyhow::{Result, bail};
use std::collections::{HashMap, HashSet};

const EXT_MAGIC: u16 = 0xF30A;
const JBD2_MAGIC: u32 = 0xC03B_3998;
const ROOT_INO: u32 = 2;

pub fn is_ext(sb: &[u8]) -> bool {
    sb.len() >= 0x400 && le16(sb, 0x38) == 0xEF53
}

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}

pub struct Ext4<'a> {
    src: &'a Source,
    vol: u64,
    bs: u64,
    blocks: u64,
    first_data_block: u64,
    blocks_per_group: u64,
    inodes_per_group: u32,
    inodes: u32,
    inode_size: usize,
    journal_ino: u32,
    /// Per group: (block bitmap, inode bitmap, inode table, flags).
    groups: Vec<(u64, u64, u64, u16)>,
}

#[derive(Clone)]
struct Inode {
    mode: u16,
    size: u64,
    mtime: u32,
    crtime: u32,
    dtime: u32,
    links: u16,
    flags: u32,
    block: [u8; 60],
}

impl Inode {
    fn parse(b: &[u8]) -> Option<Inode> {
        if b.len() < 0x80 {
            return None;
        }
        Some(Inode {
            mode: le16(b, 0),
            size: le32(b, 4) as u64 | (le32(b, 0x6C) as u64) << 32,
            mtime: le32(b, 0x10),
            dtime: le32(b, 0x14),
            links: le16(b, 0x1A),
            flags: le32(b, 0x20),
            block: b[0x28..0x28 + 60].try_into().unwrap(),
            crtime: if b.len() >= 0x94 { le32(b, 0x90) } else { 0 },
        })
    }
    fn is_reg(&self) -> bool {
        self.mode & 0xF000 == 0x8000
    }
    fn is_dir(&self) -> bool {
        self.mode & 0xF000 == 0x4000
    }
    fn uses_extents(&self) -> bool {
        self.flags & 0x8_0000 != 0
    }
}

fn time(t: u32) -> Option<String> {
    (t != 0).then(|| crate::jdr::format_time(t as i64 * 1000)[..19].to_string())
}

/// Older copies of file-system blocks found in the journal.
#[derive(Default)]
struct Journal {
    /// fs block → (transaction sequence, byte offset of the copy in the source).
    copies: HashMap<u64, Vec<(u32, u64)>>,
}

impl<'a> Ext4<'a> {
    pub fn open(src: &'a Source, vol: &Volume) -> Result<Self> {
        let sb = src.read_vec(vol.offset + 1024, 1024)?;
        if !is_ext(&sb) {
            bail!("not an ext2/3/4 super block");
        }
        let log_bs = le32(&sb, 0x18);
        if log_bs > 6 {
            bail!("ext: implausible block size");
        }
        let bs = 1024u64 << log_bs;
        let incompat = le32(&sb, 0x60);
        let ro_compat = le32(&sb, 0x64);
        if ro_compat & 0x200 != 0 {
            bail!("ext4 bigalloc volumes are not supported yet");
        }
        if incompat & 0x10 != 0 {
            bail!("ext4 meta_bg volumes are not supported yet");
        }
        let is64 = incompat & 0x80 != 0;
        let blocks = le32(&sb, 4) as u64
            | if is64 {
                (le32(&sb, 0x150) as u64) << 32
            } else {
                0
            };
        let first_data_block = le32(&sb, 0x14) as u64;
        let blocks_per_group = le32(&sb, 0x20) as u64;
        let inodes_per_group = le32(&sb, 0x28);
        let rev = le32(&sb, 0x4C);
        let inode_size = if rev >= 1 {
            le16(&sb, 0x58) as usize
        } else {
            128
        };
        if blocks_per_group == 0 || inodes_per_group == 0 || !(128..=4096).contains(&inode_size) {
            bail!("ext: implausible super block");
        }
        let desc = if is64 {
            (le16(&sb, 0xFE) as usize).max(32)
        } else {
            32
        };
        let ngroups = (blocks - first_data_block).div_ceil(blocks_per_group);
        if ngroups > 1 << 22 {
            bail!("ext: implausible group count");
        }
        let gdt = src.read_vec(
            vol.offset + (first_data_block + 1) * bs,
            ngroups as usize * desc,
        )?;
        let mut groups = Vec::with_capacity(ngroups as usize);
        for g in gdt.chunks_exact(desc) {
            let hi = |o: usize| {
                if desc >= 64 {
                    (le32(g, o) as u64) << 32
                } else {
                    0
                }
            };
            groups.push((
                le32(g, 0) as u64 | hi(0x20),
                le32(g, 4) as u64 | hi(0x24),
                le32(g, 8) as u64 | hi(0x28),
                le16(g, 0x12),
            ));
        }
        Ok(Self {
            src,
            vol: vol.offset,
            bs,
            blocks,
            first_data_block,
            blocks_per_group,
            inodes_per_group,
            inodes: le32(&sb, 0),
            inode_size,
            journal_ino: le32(&sb, 0xE0),
            groups,
        })
    }

    fn block(&self, n: u64) -> Result<Vec<u8>> {
        self.src.read_vec(self.vol + n * self.bs, self.bs as usize)
    }

    /// (table block holding the inode, byte offset inside that block).
    fn inode_pos(&self, ino: u32) -> Option<(u64, usize)> {
        if ino == 0 || ino > self.inodes {
            return None;
        }
        let g = ((ino - 1) / self.inodes_per_group) as usize;
        let idx = ((ino - 1) % self.inodes_per_group) as u64;
        let table = self.groups.get(g)?.2;
        let byte = idx * self.inode_size as u64;
        Some((table + byte / self.bs, (byte % self.bs) as usize))
    }

    fn inode(&self, ino: u32) -> Result<Option<Inode>> {
        let Some((blk, off)) = self.inode_pos(ino) else {
            return Ok(None);
        };
        let b = self.block(blk)?;
        Ok(b.get(off..off + self.inode_size).and_then(Inode::parse))
    }

    fn valid_run(&self, start: u64, len: u64) -> bool {
        len > 0 && start >= self.first_data_block && start + len <= self.blocks
    }

    /// Block runs of a file in order: (first block, count). `None` if the
    /// block list is empty or damaged. `jr` supplies older copies of extent
    /// tree blocks that have since been cleared.
    fn runs(&self, ino: &Inode, jr: Option<&Journal>) -> Result<Option<Vec<(u64, u64)>>> {
        let mut out = vec![];
        let ok = if ino.uses_extents() {
            self.extent_node(&ino.block, 0, jr, &mut out)?
        } else {
            self.block_map(ino, &mut out)?
        };
        Ok((ok && !out.is_empty()).then_some(out))
    }

    fn extent_node(
        &self,
        node: &[u8],
        level: u32,
        jr: Option<&Journal>,
        out: &mut Vec<(u64, u64)>,
    ) -> Result<bool> {
        if node.len() < 12 || le16(node, 0) != EXT_MAGIC || level > 5 {
            return Ok(false);
        }
        let n = le16(node, 2) as usize;
        let depth = le16(node, 6);
        if n == 0 || 12 + n * 12 > node.len() {
            return Ok(false);
        }
        for e in node[12..12 + n * 12].chunks_exact(12) {
            if depth == 0 {
                let mut len = le16(e, 4) as u64;
                if len > 32768 {
                    len -= 32768; // allocated but not yet written
                }
                let start = le32(e, 8) as u64 | (le16(e, 6) as u64) << 32;
                if !self.valid_run(start, len) {
                    return Ok(false);
                }
                out.push((start, len));
            } else {
                let child = le32(e, 4) as u64 | (le16(e, 8) as u64) << 32;
                if !self.valid_run(child, 1) {
                    return Ok(false);
                }
                let before = out.len();
                let b = self.block(child)?;
                if !self.extent_node(&b, level + 1, jr, out)? {
                    out.truncate(before);
                    // The block on disk was cleared: try its journal copies,
                    // newest first.
                    let mut found = false;
                    if let Some(c) = jr.and_then(|j| j.copies.get(&child)) {
                        for &(_, pos) in c.iter().rev() {
                            let b = self.src.read_vec(pos, self.bs as usize)?;
                            if self.extent_node(&b, level + 1, None, out)? {
                                found = true;
                                break;
                            }
                            out.truncate(before);
                        }
                    }
                    if !found {
                        return Ok(false);
                    }
                }
            }
        }
        Ok(true)
    }

    /// ext2/ext3 block map: 12 direct pointers, then single, double and
    /// triple indirect blocks.
    fn block_map(&self, ino: &Inode, out: &mut Vec<(u64, u64)>) -> Result<bool> {
        let need = ino.size.div_ceil(self.bs);
        let mut left = need;
        let mut push = |b: u64, out: &mut Vec<(u64, u64)>, left: &mut u64| -> bool {
            if *left == 0 {
                return true;
            }
            if !self.valid_run(b, 1) {
                return false;
            }
            match out.last_mut() {
                Some(l) if l.0 + l.1 == b => l.1 += 1,
                _ => out.push((b, 1)),
            }
            *left -= 1;
            true
        };
        for k in 0..12 {
            if !push(le32(&ino.block, k * 4) as u64, out, &mut left) {
                return Ok(false);
            }
        }
        for (k, depth) in [(12usize, 1u32), (13, 2), (14, 3)] {
            if left == 0 {
                break;
            }
            let p = le32(&ino.block, k * 4) as u64;
            if !self.indirect(p, depth, out, &mut left, &mut push)? {
                return Ok(false);
            }
        }
        Ok(left == 0)
    }

    fn indirect(
        &self,
        blk: u64,
        depth: u32,
        out: &mut Vec<(u64, u64)>,
        left: &mut u64,
        push: &mut impl FnMut(u64, &mut Vec<(u64, u64)>, &mut u64) -> bool,
    ) -> Result<bool> {
        if !self.valid_run(blk, 1) {
            return Ok(false);
        }
        let b = self.block(blk)?;
        for p in b.chunks_exact(4) {
            if *left == 0 {
                break;
            }
            let p = le32(p, 0) as u64;
            let ok = if depth == 1 {
                push(p, out, left)
            } else {
                self.indirect(p, depth - 1, out, left, push)?
            };
            if !ok {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn extents(&self, runs: &[(u64, u64)], size: u64) -> Vec<(u64, u64)> {
        let mut out = vec![];
        let mut left = size;
        for &(s, n) in runs {
            if left == 0 {
                break;
            }
            let len = (n * self.bs).min(left);
            let a = self.vol + s * self.bs;
            out.push((a, a + len));
            left -= len;
        }
        out
    }

    /// Allocated blocks as absolute byte ranges, and a test for one block.
    fn allocation(&self) -> Result<(Ranges, Vec<Vec<u8>>)> {
        let mut r = Ranges::new();
        let mut maps = Vec::with_capacity(self.groups.len());
        for (g, &(bb, _, _, flags)) in self.groups.iter().enumerate() {
            let first = self.first_data_block + g as u64 * self.blocks_per_group;
            let count = self.blocks_per_group.min(self.blocks - first);
            // BLOCK_UNINIT: the bitmap was never written; nothing but the
            // group's own metadata is in use.
            let map = if flags & 2 != 0 || !self.valid_run(bb, 1) {
                vec![0u8; self.bs as usize]
            } else {
                self.block(bb)?
            };
            let mut run: Option<u64> = None;
            for i in 0..count {
                let set = map
                    .get((i / 8) as usize)
                    .is_some_and(|b| b & (1 << (i % 8)) != 0);
                match (set, run) {
                    (true, None) => run = Some(i),
                    (false, Some(s)) => {
                        r.add(
                            self.vol + (first + s) * self.bs,
                            self.vol + (first + i) * self.bs,
                        );
                        run = None;
                    }
                    _ => {}
                }
            }
            if let Some(s) = run {
                r.add(
                    self.vol + (first + s) * self.bs,
                    self.vol + (first + count) * self.bs,
                );
            }
            maps.push(map);
        }
        // Super block and group descriptors at the start.
        r.add(self.vol, self.vol + (self.first_data_block + 2) * self.bs);
        r.normalize();
        Ok((r, maps))
    }

    fn in_use(&self, maps: &[Vec<u8>], blk: u64) -> bool {
        if blk < self.first_data_block {
            return true;
        }
        let rel = blk - self.first_data_block;
        let (g, i) = (
            (rel / self.blocks_per_group) as usize,
            rel % self.blocks_per_group,
        );
        maps.get(g)
            .and_then(|m| m.get((i / 8) as usize))
            .is_some_and(|b| b & (1 << (i % 8)) != 0)
    }

    /// Reads the journal and indexes every block copy in it.
    fn journal(&self) -> Result<Journal> {
        let mut j = Journal::default();
        let Some(ino) = self.inode(self.journal_ino)? else {
            return Ok(j);
        };
        let Some(runs) = self.runs(&ino, None)? else {
            return Ok(j);
        };
        // Journal block index → fs block.
        let mut map: Vec<u64> = vec![];
        for &(s, n) in &runs {
            if map.len() as u64 + n > 1 << 22 {
                break; // 16 GiB of 4K blocks: larger than any real journal
            }
            map.extend(s..s + n);
        }
        if map.is_empty() {
            return Ok(j);
        }
        let jsb = self.block(map[0])?;
        if be32(&jsb, 0) != JBD2_MAGIC || be32(&jsb, 12) as u64 != self.bs {
            return Ok(j);
        }
        let incompat = be32(&jsb, 0x28);
        let (is64, v2, v3) = (incompat & 2 != 0, incompat & 8 != 0, incompat & 0x10 != 0);
        let tag_size = if v3 {
            16
        } else if is64 {
            12
        } else {
            8
        };
        let tail = if v2 || v3 { 4 } else { 0 };
        let mut i = 1;
        while i < map.len() {
            let b = self.block(map[i])?;
            i += 1;
            // Descriptor block: lists the fs blocks whose copies follow.
            if be32(&b, 0) != JBD2_MAGIC || be32(&b, 4) != 1 {
                continue;
            }
            let seq = be32(&b, 8);
            let mut p = 12;
            let end = b.len() - tail;
            while p + tag_size <= end {
                let (lo, flags) = if v3 {
                    (be32(&b, p), be32(&b, p + 4))
                } else {
                    (be32(&b, p), u16::from_be_bytes([b[p + 6], b[p + 7]]) as u32)
                };
                let hi = if is64 { be32(&b, p + 8) as u64 } else { 0 };
                p += tag_size;
                if flags & 2 == 0 {
                    p += 16; // UUID follows unless SAME_UUID
                }
                if i >= map.len() {
                    break;
                }
                let target = lo as u64 | hi << 32;
                if target < self.blocks {
                    j.copies
                        .entry(target)
                        .or_default()
                        .push((seq, self.vol + map[i] * self.bs));
                }
                i += 1;
                if flags & 8 != 0 {
                    break; // LAST_TAG
                }
            }
        }
        for v in j.copies.values_mut() {
            v.sort_unstable();
        }
        Ok(j)
    }

    /// Directory entries of one block, including those left in the unused
    /// tail of an entry (deleted): (inode, name, deleted).
    fn dir_entries(&self, b: &[u8]) -> Vec<(u32, String, bool)> {
        let mut out = vec![];
        let parse = |p: usize, lim: usize| -> Option<(u32, usize, String)> {
            if p + 8 > lim {
                return None;
            }
            let ino = le32(b, p);
            let rec = le16(b, p + 4) as usize;
            let nl = b[p + 6] as usize;
            if rec < 8
                || !rec.is_multiple_of(4)
                || p + rec > b.len()
                || 8 + nl > rec
                || p + 8 + nl > lim
            {
                return None;
            }
            let name = String::from_utf8_lossy(&b[p + 8..p + 8 + nl]).into_owned();
            Some((ino, rec, name))
        };
        let mut p = 0;
        while p + 8 <= b.len() {
            let Some((ino, rec, name)) = parse(p, b.len()) else {
                break;
            };
            let used = if ino == 0 {
                8
            } else {
                (8 + name.len()).div_ceil(4) * 4
            };
            if ino != 0 && !name.is_empty() {
                out.push((ino, name, false));
            }
            // Entries hidden in the slack after this one.
            let mut q = p + used;
            let lim = p + rec;
            while q + 12 <= lim {
                match parse(q, lim) {
                    Some((i2, _, n2))
                        if i2 != 0
                            && i2 <= self.inodes
                            && !n2.is_empty()
                            && n2 != "."
                            && n2 != ".."
                            && !n2
                                .chars()
                                .any(|c| c.is_control() || c == '/' || c == '\u{FFFD}') =>
                    {
                        q += (8 + n2.len()).div_ceil(4) * 4;
                        out.push((i2, n2, true));
                    }
                    _ => q += 4,
                }
            }
            p += rec;
        }
        out
    }

    pub fn analyze(&self) -> Result<FsInfo> {
        let (allocated, maps) = self.allocation()?;
        let mut info = FsInfo {
            allocated,
            cluster_size: self.bs,
            cluster_count: self.blocks.min(u32::MAX as u64) as u32,
            ..Default::default()
        };
        // Walk the directory tree.
        let mut live_inodes: HashSet<u32> = HashSet::new();
        let mut deleted_names: HashMap<u32, String> = HashMap::new();
        let mut dir_blocks: Vec<(u64, String)> = vec![];
        let mut stack = vec![(ROOT_INO, String::new())];
        let mut seen: HashSet<u32> = HashSet::new();
        while let Some((dino, dpath)) = stack.pop() {
            if !seen.insert(dino) || seen.len() > 2_000_000 {
                continue;
            }
            let Some(d) = self.inode(dino)? else { continue };
            if !d.is_dir() || d.flags & 0x1000_0000 != 0 {
                continue; // not a directory, or data stored inline
            }
            let Some(runs) = self.runs(&d, None)? else {
                continue;
            };
            for &(s, n) in &runs {
                for blk in s..s + n.min(1 << 16) {
                    let b = self.block(blk)?;
                    dir_blocks.push((blk, dpath.clone()));
                    for (ino, name, deleted) in self.dir_entries(&b) {
                        if name == "." || name == ".." {
                            continue;
                        }
                        let path = format!("{dpath}/{name}");
                        if deleted {
                            deleted_names.entry(ino).or_insert(path);
                            continue;
                        }
                        let Some(i) = self.inode(ino)? else { continue };
                        if i.is_dir() {
                            stack.push((ino, path));
                        } else if i.is_reg() && i.size > 0 {
                            live_inodes.insert(ino);
                            if let Some(r) = self.runs(&i, None)? {
                                info.live.push(DeletedFile {
                                    path,
                                    size: i.size,
                                    first_cluster: r[0].0.min(u32::MAX as u64) as u32,
                                    extents: self.extents(&r, i.size),
                                    method: "inode extents",
                                    modified: time(i.mtime),
                                    created: time(i.crtime),
                                });
                            }
                        }
                    }
                }
            }
        }
        info.live_files = info.live.len();
        // A name left in directory slack may belong to a file that still
        // exists under another name (rename): not deleted.
        deleted_names.retain(|ino, _| !live_inodes.contains(ino));

        // Deleted files, from two places: the inode itself (block list still
        // there: ext2, or a file removed without the kernel), and older
        // copies of inode table blocks in the journal.
        let jr = self.journal()?;
        // Newer kernels wipe a directory entry when the file is deleted;
        // older copies of the directory block in the journal still name it.
        for (blk, dpath) in &dir_blocks {
            for &(_, pos) in jr.copies.get(blk).map(|v| v.as_slice()).unwrap_or(&[]) {
                let b = self.src.read_vec(pos, self.bs as usize)?;
                for (ino, name, _) in self.dir_entries(&b) {
                    if name != "." && name != ".." && !live_inodes.contains(&ino) {
                        deleted_names.insert(ino, format!("{dpath}/{name}"));
                    }
                }
            }
        }
        let mut found: HashMap<u32, DeletedFile> = HashMap::new();
        let mut table_of: HashMap<u64, (usize, u64)> = HashMap::new();
        let table_blocks =
            (self.inodes_per_group as u64 * self.inode_size as u64).div_ceil(self.bs);
        for (g, gr) in self.groups.iter().enumerate() {
            for k in 0..table_blocks {
                table_of.insert(gr.2 + k, (g, k));
            }
        }
        let per_block = (self.bs as usize / self.inode_size) as u32;
        let consider = |ino_no: u32,
                        i: &Inode,
                        method: &'static str,
                        found: &mut HashMap<u32, DeletedFile>|
         -> Result<()> {
            if !i.is_reg() || i.size == 0 || i.size > self.blocks * self.bs {
                return Ok(());
            }
            let Some(r) = self.runs(i, Some(&jr))? else {
                return Ok(());
            };
            let total: u64 = r.iter().map(|x| x.1).sum();
            if total < i.size.div_ceil(self.bs) {
                return Ok(());
            }
            let reused = r
                .iter()
                .flat_map(|&(s, n)| s..s + n)
                .filter(|&b| self.in_use(&maps, b))
                .count() as u64;
            // Mostly reused blocks now belong to another file.
            if reused * 2 > total {
                return Ok(());
            }
            let path = deleted_names
                .get(&ino_no)
                .cloned()
                .unwrap_or_else(|| format!("/_deleted_inode_{ino_no}"));
            found.insert(
                ino_no,
                DeletedFile {
                    path,
                    size: i.size,
                    first_cluster: r[0].0.min(u32::MAX as u64) as u32,
                    extents: self.extents(&r, i.size),
                    method: if reused > 0 {
                        "ext journal inode copy; some blocks reused since (content may be partly overwritten)"
                    } else {
                        method
                    },
                    modified: time(i.mtime),
                    created: time(i.crtime),
                },
            );
            Ok(())
        };
        // 1. Journal copies, oldest first so the newest usable copy wins.
        let mut tables: Vec<(u32, u64, u64)> = vec![];
        for (&blk, copies) in &jr.copies {
            if table_of.contains_key(&blk) {
                tables.extend(copies.iter().map(|&(seq, pos)| (seq, pos, blk)));
            }
        }
        tables.sort_unstable();
        for (_, pos, blk) in tables {
            let (g, k) = table_of[&blk];
            let b = self.src.read_vec(pos, self.bs as usize)?;
            for n in 0..per_block {
                let ino_no = g as u32 * self.inodes_per_group + k as u32 * per_block + n + 1;
                if ino_no > self.inodes || live_inodes.contains(&ino_no) {
                    continue;
                }
                let o = n as usize * self.inode_size;
                let Some(i) = b.get(o..o + self.inode_size).and_then(Inode::parse) else {
                    continue;
                };
                if i.dtime != 0 || i.links == 0 {
                    continue; // already deleted in this copy
                }
                // Still the same file today? Then it is live, not deleted.
                if let Some(cur) = self.inode(ino_no)?
                    && cur.dtime == 0
                    && cur.links > 0
                    && cur.is_reg()
                {
                    continue;
                }
                consider(
                    ino_no,
                    &i,
                    "ext journal inode copy (extents from before deletion); blocks free now",
                    &mut found,
                )?;
            }
        }
        // 2. Inodes marked deleted whose block list is still in place.
        for (g, gr) in self.groups.iter().enumerate() {
            if gr.3 & 1 != 0 {
                continue; // INODE_UNINIT: table never used
            }
            for k in 0..table_blocks {
                if !self.valid_run(gr.2 + k, 1) {
                    break;
                }
                let b = self.block(gr.2 + k)?;
                if b.iter().all(|&x| x == 0) {
                    continue;
                }
                for n in 0..per_block {
                    let ino_no = g as u32 * self.inodes_per_group + k as u32 * per_block + n + 1;
                    if ino_no > self.inodes || found.contains_key(&ino_no) {
                        continue;
                    }
                    let o = n as usize * self.inode_size;
                    let Some(i) = b.get(o..o + self.inode_size).and_then(Inode::parse) else {
                        continue;
                    };
                    if i.dtime == 0 && i.links > 0 {
                        continue;
                    }
                    consider(
                        ino_no,
                        &i,
                        "ext inode still holds its block list",
                        &mut found,
                    )?;
                }
            }
        }
        let mut deleted: Vec<DeletedFile> = found.into_values().collect();
        deleted.sort_by(|a, b| a.path.cmp(&b.path));
        info.deleted = deleted;
        Ok(info)
    }
}
