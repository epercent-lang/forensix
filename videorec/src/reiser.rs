//! ReiserFS 3.5 / 3.6 metadata: live files, allocated space, and deleted
//! files whose items survive in older tree leaves.
//!
//! Layout as defined in the Linux kernel source fs/reiserfs/reiserfs.h
//! (kernels up to 6.12): super block at 64 KiB, block bitmaps, one balanced
//! tree whose leaves hold stat data, directory, indirect (block pointer)
//! and direct (file tail) items. Only reading is implemented, for the
//! standard layout with the journal inside the volume.
//!
//! Deleting a file removes its items from the tree, but leaves that were
//! rewritten or released keep their old contents: copies inside the journal
//! and blocks that are now free. Those are searched for leaf nodes; a file
//! the current tree no longer has is rebuilt from its stat data and block
//! pointers when the pointers cover its whole size and every block is still
//! free. Free blocks are read only on volumes up to 64 GiB (set
//! VIDEOREC_FS_SCAN_GB to change), the journal always.

use crate::fs::{DeletedFile, FsInfo};
use crate::io::{Source, Volume, le16, le32, le64};
use crate::ranges::Ranges;
use crate::xfs::unix_time;
use anyhow::{Result, bail};
use std::collections::{BTreeMap, HashMap, HashSet};

const SUPER: u64 = 0x10000;

pub fn is_reiser(sb: &[u8]) -> bool {
    sb.len() >= 64
        && (sb[52..].starts_with(b"ReIsErFs")
            || sb[52..].starts_with(b"ReIsEr2Fs")
            || sb[52..].starts_with(b"ReIsEr3Fs"))
        && le16(sb, 44).is_power_of_two()
        && (512..=8192).contains(&le16(sb, 44))
}

type Key = (u32, u32);

/// What is known about one object (file or directory).
#[derive(Default)]
struct Obj {
    mode: u16,
    size: u64,
    mtime: Option<String>,
    /// File block index -> disk block.
    blocks: BTreeMap<u64, u32>,
    /// Tails: byte offset in the file -> (absolute position, length).
    tails: BTreeMap<u64, (u64, u64)>,
}

#[derive(Default)]
struct Tree {
    objs: HashMap<Key, Obj>,
    /// Child -> (directory, name).
    names: HashMap<Key, (Key, String)>,
}

pub struct Reiser<'a> {
    src: &'a Source,
    vol: u64,
    bs: u64,
    blocks: u64,
    root: u64,
    journal: (u64, u64),
}

impl<'a> Reiser<'a> {
    pub fn open(src: &'a Source, vol: &Volume) -> Result<Self> {
        let sb = src.read_vec(vol.offset + SUPER, 204)?;
        if !is_reiser(&sb) {
            bail!("not a ReiserFS volume");
        }
        let in_volume = le32(&sb, 16) == 0;
        Ok(Reiser {
            src,
            vol: vol.offset,
            bs: le16(&sb, 44) as u64,
            blocks: le32(&sb, 0) as u64,
            root: le32(&sb, 8) as u64,
            journal: if in_volume {
                (le32(&sb, 12) as u64, le32(&sb, 20) as u64 + 1)
            } else {
                (0, 0)
            },
        })
    }

    fn block(&self, n: u64) -> Result<Vec<u8>> {
        if n >= self.blocks {
            bail!("block outside the volume");
        }
        self.src.read_vec(self.vol + n * self.bs, self.bs as usize)
    }

    /// Adds the items of a leaf to `t`. Returns false when `b` is not a
    /// well-formed leaf. `skip` lists objects not to collect.
    fn leaf(&self, b: &[u8], at: u64, t: &mut Tree, skip: Option<&HashSet<Key>>) -> bool {
        let bs = b.len();
        let nr = le16(b, 2) as usize;
        if le16(b, 0) != 1 || nr == 0 || 24 + 24 * nr > bs {
            return false;
        }
        // Item bodies are packed from the end of the block downwards.
        let mut end = bs;
        for i in 0..nr {
            let h = 24 + 24 * i;
            let (len, loc) = (le16(b, h + 18) as usize, le16(b, h + 20) as usize);
            if loc + len != end || loc < 24 + 24 * nr || le16(b, h + 22) > 1 {
                return false;
            }
            end = loc;
        }
        if le16(b, 4) as usize != end - (24 + 24 * nr) {
            return false;
        }
        for i in 0..nr {
            let h = 24 + 24 * i;
            let key: Key = (le32(b, h), le32(b, h + 4));
            let (len, loc) = (le16(b, h + 18) as usize, le16(b, h + 20) as usize);
            let (off, kind) = if le16(b, h + 22) == 1 {
                let v = le64(b, h + 8);
                (v & ((1 << 60) - 1), (v >> 60) as u32)
            } else {
                let kind = match le32(b, h + 12) {
                    0 => 0,
                    0xFFFF_FFFE => 1,
                    0xFFFF_FFFF => 2,
                    500 => 3,
                    _ => 15,
                };
                (le32(b, h + 8) as u64, kind)
            };
            let body = &b[loc..loc + len];
            if kind == 3 {
                // Directory item: entry heads, names packed from the end.
                let n = le16(b, h + 16) as usize;
                let mut name_end = len;
                for e in 0..n {
                    if 16 * (e + 1) > len {
                        break;
                    }
                    let l = le16(body, 16 * e + 12) as usize;
                    if l < 16 * n || l > name_end {
                        break;
                    }
                    let raw = &body[l..name_end];
                    name_end = l;
                    let raw = &raw[..raw.iter().position(|&c| c == 0).unwrap_or(raw.len())];
                    let name = String::from_utf8_lossy(raw).replace('/', "_");
                    let child: Key = (le32(body, 16 * e + 4), le32(body, 16 * e + 8));
                    if name != "." && name != ".." && !name.is_empty() {
                        t.names.entry(child).or_insert((key, name));
                    }
                }
                continue;
            }
            if skip.is_some_and(|s| s.contains(&key)) {
                continue;
            }
            match kind {
                0 if len == 44 || len == 32 => {
                    let o = t.objs.entry(key).or_default();
                    let (size, mtime) = if len == 44 {
                        (le64(body, 8), le32(body, 28))
                    } else {
                        (le32(body, 8) as u64, le32(body, 16))
                    };
                    // Of several images of a file, the largest is kept.
                    if size >= o.size {
                        o.mode = le16(body, 0);
                        o.size = size;
                        o.mtime = unix_time(mtime as i64);
                    }
                }
                1 if off >= 1 && (off - 1) % self.bs == 0 => {
                    let o = t.objs.entry(key).or_default();
                    let first = (off - 1) / self.bs;
                    for k in 0..len / 4 {
                        o.blocks.insert(first + k as u64, le32(body, 4 * k));
                    }
                }
                2 if off >= 1 => {
                    let o = t.objs.entry(key).or_default();
                    o.tails
                        .insert(off - 1, (self.vol + at * self.bs + loc as u64, len as u64));
                }
                _ => {}
            }
        }
        true
    }

    fn walk(&self, n: u64, depth: u32, t: &mut Tree, seen: &mut HashSet<u64>) {
        if depth > 8 || !seen.insert(n) {
            return;
        }
        let Ok(b) = self.block(n) else { return };
        let (level, nr) = (le16(&b, 0), le16(&b, 2) as usize);
        if level == 1 {
            self.leaf(&b, n, t, None);
        } else if (2..=7).contains(&level) && 24 + 16 * nr + 8 * (nr + 1) <= b.len() {
            for i in 0..=nr {
                self.walk(le32(&b, 24 + 16 * nr + 8 * i) as u64, depth + 1, t, seen);
            }
        }
    }

    fn allocation(&self) -> Result<Ranges> {
        let mut r = Ranges::new();
        let per = self.bs * 8;
        let mut run: Option<u64> = None;
        for k in 0..self.blocks.div_ceil(per) {
            let at = if k == 0 { SUPER / self.bs + 1 } else { k * per };
            // An unreadable bitmap counts as allocated.
            let map = self.block(at).unwrap_or_else(|_| vec![0xFF; self.bs as usize]);
            for (i, &v) in map.iter().enumerate() {
                for bit in 0..8u64 {
                    let blk = k * per + i as u64 * 8 + bit;
                    if blk >= self.blocks {
                        break;
                    }
                    match (v & (1 << bit) != 0, run) {
                        (true, None) => run = Some(blk),
                        (false, Some(s)) => {
                            r.add(self.vol + s * self.bs, self.vol + blk * self.bs);
                            run = None;
                        }
                        _ => {}
                    }
                }
            }
        }
        if let Some(s) = run {
            r.add(self.vol + s * self.bs, self.vol + self.blocks * self.bs);
        }
        r.normalize();
        Ok(r)
    }

    /// Byte extents of an object's first `size` bytes; `None` when block
    /// pointers are missing.
    fn extents(&self, o: &Obj) -> Option<Vec<(u64, u64)>> {
        let mut out: Vec<(u64, u64)> = vec![];
        let mut push = |a: u64, l: u64| match out.last_mut() {
            Some(x) if x.1 == a => x.1 += l,
            _ => out.push((a, a + l)),
        };
        let mut pos = 0u64;
        while pos < o.size {
            if let Some(&(abs, len)) = o.tails.get(&pos) {
                let l = len.min(o.size - pos);
                push(abs, l);
                pos += len;
                continue;
            }
            if pos % self.bs != 0 {
                return None;
            }
            let blk = *o.blocks.get(&(pos / self.bs))? as u64;
            if blk == 0 || blk >= self.blocks {
                return None;
            }
            push(self.vol + blk * self.bs, self.bs.min(o.size - pos));
            pos += self.bs;
        }
        Some(out)
    }

    pub fn analyze(&self) -> Result<FsInfo> {
        let allocated = self.allocation()?;
        let mut info = FsInfo {
            cluster_size: self.bs,
            cluster_count: self.blocks.min(u32::MAX as u64) as u32,
            ..Default::default()
        };
        let mut live = Tree::default();
        let mut tree_blocks = HashSet::new();
        self.walk(self.root, 0, &mut live, &mut tree_blocks);

        let path_of = |names: &[&HashMap<Key, (Key, String)>], key: Key| {
            let mut parts = vec![];
            let (mut k, mut guard) = (key, 0);
            while k != (1, 2) && guard < 256 {
                match names.iter().find_map(|m| m.get(&k)) {
                    Some((dir, name)) => {
                        parts.push(name.clone());
                        k = *dir;
                    }
                    None => {
                        parts.push(format!("reiserfs_object_{}_{}", k.0, k.1));
                        break;
                    }
                }
                guard += 1;
            }
            parts.reverse();
            format!("/{}", parts.join("/"))
        };
        for (&key, o) in &live.objs {
            if o.mode & 0xF000 != 0x8000 || o.size == 0 {
                continue;
            }
            if let Some(extents) = self.extents(o) {
                info.live.push(DeletedFile {
                    path: path_of(&[&live.names], key),
                    size: o.size,
                    first_cluster: 0,
                    extents,
                    method: "tree items",
                    modified: o.mtime.clone(),
                    created: None,
                });
            }
        }
        info.live.sort_by(|a, b| a.path.cmp(&b.path));
        info.live_files = info.live.len();

        // Older leaves: the journal, and free blocks on small volumes.
        let live_keys: HashSet<Key> = live.objs.keys().copied().collect();
        let mut old = Tree::default();
        let limit_gb = std::env::var("VIDEOREC_FS_SCAN_GB")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(64);
        let mut areas = vec![(
            self.vol + self.journal.0 * self.bs,
            self.vol + (self.journal.0 + self.journal.1).min(self.blocks) * self.bs,
        )];
        if self.blocks * self.bs <= limit_gb << 30 {
            areas.extend(allocated.gaps(self.vol, self.vol + self.blocks * self.bs));
        }
        let step = (4u64 << 20) / self.bs * self.bs;
        for (a, b) in areas {
            let mut p = a;
            while p < b {
                let n = (b - p).min(step) as usize;
                let Ok(buf) = self.src.read_vec(p, n) else {
                    p += n as u64;
                    continue;
                };
                for (i, blk) in buf.chunks_exact(self.bs as usize).enumerate() {
                    if blk[0] == 1 && blk[1] == 0 {
                        let at = (p - self.vol) / self.bs + i as u64;
                        if !tree_blocks.contains(&at) {
                            self.leaf(blk, at, &mut old, Some(&live_keys));
                        }
                    }
                }
                p += n as u64;
            }
        }
        for (&key, o) in &old.objs {
            if o.mode & 0xF000 != 0x8000 || o.size == 0 {
                continue;
            }
            let Some(extents) = self.extents(o) else {
                continue;
            };
            // Tails lie inside old leaves (not allocated either when the
            // leaf was released; journal copies are exempt).
            let jr = areas_journal(self);
            if extents.iter().any(|&(a, b)| {
                !(jr.0 <= a && b <= jr.1) && allocated.gaps(a, b) != vec![(a, b)]
            }) {
                continue;
            }
            info.deleted.push(DeletedFile {
                path: path_of(&[&old.names, &live.names], key),
                size: o.size,
                first_cluster: 0,
                extents,
                method: "ReiserFS: items in an older tree leaf (journal or free block); blocks still free",
                modified: o.mtime.clone(),
                created: None,
            });
        }
        info.deleted.sort_by(|a, b| a.path.cmp(&b.path));
        info.allocated = allocated;
        Ok(info)
    }
}

fn areas_journal(r: &Reiser) -> (u64, u64) {
    (
        r.vol + r.journal.0 * r.bs,
        r.vol + (r.journal.0 + r.journal.1) * r.bs,
    )
}
