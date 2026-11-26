//! Btrfs metadata: live files, allocated space, and deleted files whose
//! records survive in older tree blocks.
//!
//! Layout as documented in the kernel's Btrfs documentation ("On-disk
//! format") and include/uapi/linux/btrfs_tree.h: super block at 64 KiB,
//! chunk tree (logical to physical addresses), root tree, file trees with
//! INODE_ITEM / INODE_REF / EXTENT_DATA items, extent tree. Only reading is
//! implemented, for a single device without RAID0/10/5/6 striping.
//!
//! Btrfs never changes a tree block in place: a change writes a new copy
//! and the old block stays on disk until its space is reused. A file tree
//! leaf written before a file was deleted therefore still lists the file's
//! name, size and extents. Such leaves are found by reading every block of
//! the metadata chunks, accepted only with a correct checksum, and used for
//! files that the current tree no longer has and whose data blocks are not
//! allocated to anything else.

use crate::fs::{DeletedFile, FsInfo};
use crate::io::{Source, Volume, le16, le32, le64};
use crate::ranges::Ranges;
use anyhow::{Result, bail};
use std::collections::{BTreeMap, HashMap, HashSet};

const SUPER: u64 = 0x10000;
const HDR: usize = 0x65;

pub fn is_btrfs(sb: &[u8]) -> bool {
    sb.len() >= 0x48 && &sb[0x40..0x48] == b"_BHRfS_M"
}

fn crc32c(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = (c >> 1) ^ (0x82F6_3B78 & (!(c & 1)).wrapping_add(1));
        }
    }
    !c
}

/// A chunk: logical start, length, physical start of its first stripe, type.
#[derive(Clone, Copy)]
struct Chunk {
    logical: u64,
    len: u64,
    phys: u64,
    kind: u64,
}

pub struct Btrfs<'a> {
    src: &'a Source,
    vol: u64,
    node: usize,
    fsid: [u8; 16],
    chunks: Vec<Chunk>,
    root: u64,
    csum_crc: bool,
}

struct Leaf<'b> {
    b: &'b [u8],
}

impl<'b> Leaf<'b> {
    /// (objectid, type, offset, data) of every item.
    fn items(&self) -> Vec<(u64, u8, u64, &'b [u8])> {
        let n = le32(self.b, 0x60) as usize;
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let p = HDR + i * 25;
            if p + 25 > self.b.len() {
                break;
            }
            let (off, size) = (le32(self.b, p + 17) as usize, le32(self.b, p + 21) as usize);
            let Some(d) = self.b.get(HDR + off..HDR + off + size) else {
                break;
            };
            out.push((le64(self.b, p), self.b[p + 8], le64(self.b, p + 9), d));
        }
        out
    }
}

/// What is known about one file from tree items.
#[derive(Default, Clone)]
struct File {
    size: u64,
    mode: u32,
    mtime: i64,
    otime: i64,
    name: Option<(u64, String)>,
    /// file offset -> (logical address, bytes), `None` for data that cannot
    /// be read as it is (compressed, inline, hole).
    extents: BTreeMap<u64, Option<(u64, u64)>>,
    /// Generation of the leaf each part came from.
    gen_inode: u64,
}

fn time(secs: i64) -> Option<String> {
    (946_684_800..4_102_444_800)
        .contains(&secs)
        .then(|| crate::jdr::format_time(secs * 1000)[..19].to_string())
}

impl<'a> Btrfs<'a> {
    pub fn open(src: &'a Source, vol: &Volume) -> Result<Self> {
        let sb = src.read_vec(vol.offset + SUPER, 4096)?;
        if !is_btrfs(&sb) {
            bail!("not a Btrfs super block");
        }
        if le64(&sb, 0x88) != 1 {
            bail!("Btrfs on several devices is not supported");
        }
        let mut x = Btrfs {
            src,
            vol: vol.offset,
            node: le32(&sb, 0x94) as usize,
            fsid: sb[0x20..0x30].try_into().unwrap(),
            chunks: vec![],
            root: le64(&sb, 0x50),
            csum_crc: le16(&sb, 0xC4) == 0,
        };
        if !(4096..=65536).contains(&x.node) {
            bail!("Btrfs node size {} not supported", x.node);
        }
        // Chunks listed in the super block, enough to read the chunk tree.
        let n = (le32(&sb, 0xA0) as usize).min(2048);
        let arr = &sb[0x32B..0x32B + n];
        let mut p = 0;
        while p + 17 + 48 <= arr.len() {
            let logical = le64(arr, p + 9);
            let used = x.add_chunk(logical, &arr[p + 17..])?;
            p += 17 + used;
        }
        // The rest of the chunks.
        let mut more = vec![];
        x.walk(le64(&sb, 0x58), &mut HashSet::new(), &mut |_, items| {
            for (_, t, off, d) in items {
                if t == 228 {
                    more.push((off, d.to_vec()));
                }
            }
        })?;
        for (logical, d) in more {
            if !x.chunks.iter().any(|c| c.logical == logical) {
                x.add_chunk(logical, &d)?;
            }
        }
        x.chunks.sort_by_key(|c| c.logical);
        Ok(x)
    }

    fn add_chunk(&mut self, logical: u64, d: &[u8]) -> Result<usize> {
        if d.len() < 48 + 32 {
            bail!("Btrfs chunk item too short");
        }
        let (len, kind, stripes) = (le64(d, 0), le64(d, 24), le16(d, 44) as usize);
        if kind & (0x8 | 0x40 | 0x80 | 0x100) != 0 {
            bail!("Btrfs RAID0/10/5/6 chunks are not supported");
        }
        if stripes == 0 {
            bail!("Btrfs chunk without stripes");
        }
        self.chunks.push(Chunk {
            logical,
            len,
            phys: le64(d, 48 + 8),
            kind,
        });
        Ok(48 + 32 * stripes)
    }

    /// Absolute byte offset of a logical address, if `len` bytes map there.
    fn map(&self, logical: u64, len: u64) -> Option<u64> {
        let c = self
            .chunks
            .iter()
            .find(|c| c.logical <= logical && logical + len <= c.logical + c.len)?;
        Some(self.vol + c.phys + (logical - c.logical))
    }

    /// Reads the tree block at a logical address; `None` unless it is a
    /// block of this file system with the right address (and checksum).
    fn node(&self, logical: u64) -> Result<Option<Vec<u8>>> {
        let Some(at) = self.map(logical, self.node as u64) else {
            return Ok(None);
        };
        let b = self.src.read_vec(at, self.node)?;
        if b.len() < self.node
            || b[0x20..0x30] != self.fsid
            || le64(&b, 0x30) != logical
            || (self.csum_crc && le32(&b, 0) != crc32c(&b[0x20..]))
        {
            return Ok(None);
        }
        Ok(Some(b))
    }

    /// Visits every leaf under `root`: `f(owner tree, items)`.
    fn walk(
        &self,
        root: u64,
        seen: &mut HashSet<u64>,
        f: &mut dyn FnMut(u64, Vec<(u64, u8, u64, &[u8])>),
    ) -> Result<()> {
        let mut stack = vec![root];
        while let Some(l) = stack.pop() {
            if !seen.insert(l) || seen.len() > 50_000_000 {
                continue;
            }
            let Some(b) = self.node(l)? else { continue };
            let n = le32(&b, 0x60) as usize;
            if b[0x64] == 0 {
                f(le64(&b, 0x58), Leaf { b: &b }.items());
            } else {
                for i in 0..n.min((self.node - HDR) / 33) {
                    stack.push(le64(&b, HDR + i * 33 + 17));
                }
            }
        }
        Ok(())
    }

    /// Adds the file tree items of one leaf to `files` (keyed by tree and
    /// inode). Newer leaves replace what older ones said.
    fn collect(
        files: &mut HashMap<(u64, u64), File>,
        owner: u64,
        leaf_gen: u64,
        items: &[(u64, u8, u64, &[u8])],
    ) {
        for &(obj, t, off, d) in items {
            let f = files.entry((owner, obj));
            match t {
                1 if d.len() >= 160 => {
                    let f = f.or_default();
                    // Size 0 is what an inode shows while it is being removed.
                    if leaf_gen >= f.gen_inode && (le64(d, 16) > 0 || f.gen_inode == 0) {
                        f.size = le64(d, 16);
                        f.mode = le32(d, 52);
                        f.mtime = le64(d, 136) as i64;
                        f.otime = le64(d, 148) as i64;
                        f.gen_inode = leaf_gen;
                    }
                }
                12 if d.len() >= 10 => {
                    let n = le16(d, 8) as usize;
                    if let Some(name) = d.get(10..10 + n) {
                        f.or_default().name =
                            Some((off, String::from_utf8_lossy(name).into_owned()));
                    }
                }
                108 if d.len() >= 21 => {
                    let plain = d[16] == 0 && d[17] == 0 && le16(d, 18) == 0;
                    let e = if d[20] == 1 && d.len() >= 53 && plain && le64(d, 21) != 0 {
                        Some((le64(d, 21) + le64(d, 37), le64(d, 45)))
                    } else {
                        None
                    };
                    f.or_default().extents.insert(off, e);
                }
                _ => {}
            }
        }
    }

    /// Absolute byte extents of a file's first `size` bytes, if all of it
    /// is plain data that follows on without holes.
    fn byte_extents(&self, f: &File) -> Option<Vec<(u64, u64)>> {
        let mut out: Vec<(u64, u64)> = vec![];
        let mut next = 0u64;
        for (&off, e) in &f.extents {
            if next >= f.size {
                break;
            }
            let (logical, len) = (*e)?;
            if off != next || len == 0 {
                return None;
            }
            let abs = self.map(logical, len)?;
            let l = len.min(f.size - next);
            match out.last_mut() {
                Some(x) if x.1 == abs => x.1 += l,
                _ => out.push((abs, abs + l)),
            }
            next += len;
        }
        (next >= f.size && !out.is_empty()).then_some(out)
    }

    pub fn analyze(&self) -> Result<FsInfo> {
        let mut info = FsInfo {
            cluster_size: 4096,
            ..Default::default()
        };
        // Root tree: where each tree starts.
        let mut seen = HashSet::new();
        let mut roots: Vec<(u64, u64)> = vec![];
        self.walk(self.root, &mut seen, &mut |_, items| {
            for (obj, t, _, d) in items {
                if t == 132 && d.len() >= 184 {
                    roots.push((obj, le64(d, 176)));
                }
            }
        })?;
        // Allocated space: every extent of the extent tree, plus the areas
        // Btrfs reserves (first MiB, super block copies).
        let mut used = Ranges::new();
        used.add(self.vol, self.vol + (1 << 20));
        for sb in [SUPER, 64 << 20, 256 << 30] {
            if sb + 4096 <= self.src.len() - self.vol {
                used.add(self.vol + sb, self.vol + sb + 4096);
            }
        }
        let mut alloc_logical = Ranges::new();
        let node = self.node as u64;
        for &(obj, at) in &roots {
            if obj != 2 {
                continue;
            }
            self.walk(at, &mut seen, &mut |_, items| {
                for (o, t, off, _) in items {
                    match t {
                        168 => alloc_logical.add(o, o + off),
                        169 => alloc_logical.add(o, o + node),
                        _ => {}
                    }
                }
            })?;
        }
        alloc_logical.normalize();
        for (a, b) in alloc_logical.iter() {
            // A range may span chunks only if they follow on physically.
            let mut p = a;
            while p < b {
                let Some(c) = self
                    .chunks
                    .iter()
                    .find(|c| c.logical <= p && p < c.logical + c.len)
                else {
                    break;
                };
                let e = b.min(c.logical + c.len);
                let abs = self.vol + c.phys + (p - c.logical);
                used.add(abs, abs + (e - p));
                p = e;
            }
        }
        used.normalize();

        // Current file trees (the default tree and subvolumes).
        let mut cur: HashMap<(u64, u64), File> = HashMap::new();
        for &(obj, at) in &roots {
            if obj == 5 || (256..u64::MAX - 256).contains(&obj) {
                self.walk(at, &mut seen, &mut |owner, items| {
                    Self::collect(&mut cur, owner, 1, &items);
                })?;
            }
        }
        let path_of = |files: &HashMap<(u64, u64), File>, tree: u64, f: &File| -> String {
            let mut parts = vec![];
            let mut at = f.name.clone();
            let mut guard = 0;
            while let Some((parent, name)) = at {
                parts.push(name);
                guard += 1;
                if parent == 256 || guard > 256 {
                    break;
                }
                at = cur
                    .get(&(tree, parent))
                    .or_else(|| files.get(&(tree, parent)))
                    .and_then(|p| p.name.clone());
            }
            parts.reverse();
            let prefix = if tree == 5 {
                String::new()
            } else {
                format!("/subvol_{tree}")
            };
            format!("{prefix}/{}", parts.join("/"))
        };
        for (&(tree, _), f) in &cur {
            if f.mode & 0xF000 != 0x8000 || f.size == 0 || f.name.is_none() {
                continue;
            }
            if let Some(extents) = self.byte_extents(f) {
                info.live.push(DeletedFile {
                    path: path_of(&cur, tree, f),
                    size: f.size,
                    first_cluster: 0,
                    extents,
                    method: "file tree extents",
                    modified: time(f.mtime),
                    created: time(f.otime),
                });
            }
        }
        info.live.sort_by(|a, b| a.path.cmp(&b.path));
        info.live_files = info.live.len();

        // Older leaves of file trees, in generation order so that newer
        // ones replace what older ones said.
        let mut old_leaves: Vec<(u64, u64)> = vec![]; // (generation, logical)
        for c in self.chunks.iter().filter(|c| c.kind & 4 != 0) {
            let mut l = c.logical;
            while l + node <= c.logical + c.len {
                if !seen.contains(&l)
                    && let Some(b) = self.node(l)?
                    && b[0x64] == 0
                {
                    let owner = le64(&b, 0x58);
                    if owner == 5 || (256..u64::MAX - 256).contains(&owner) {
                        old_leaves.push((le64(&b, 0x50), l));
                    }
                }
                l += node;
            }
        }
        old_leaves.sort_unstable();
        let mut old: HashMap<(u64, u64), File> = HashMap::new();
        for &(generation, l) in &old_leaves {
            if let Some(b) = self.node(l)? {
                Self::collect(
                    &mut old,
                    le64(&b, 0x58),
                    generation,
                    &Leaf { b: &b }.items(),
                );
            }
        }
        for (&(tree, ino), f) in &old {
            if cur.get(&(tree, ino)).is_some_and(|c| c.gen_inode > 0) {
                continue; // the file still exists
            }
            if f.mode & 0xF000 != 0x8000 || f.size == 0 {
                continue;
            }
            let Some(extents) = self.byte_extents(f) else {
                continue;
            };
            // Data allocated again belongs to something else now.
            if extents
                .iter()
                .any(|&(a, b)| used.gaps(a, b) != vec![(a, b)])
            {
                continue;
            }
            info.deleted.push(DeletedFile {
                path: if f.name.is_some() {
                    path_of(&old, tree, f)
                } else {
                    format!("/btrfs_inode_{ino}")
                },
                size: f.size,
                first_cluster: 0,
                extents,
                method: "Btrfs: older copy of a file tree block (name, size, extents); blocks still free",
                modified: time(f.mtime),
                created: time(f.otime),
            });
        }
        info.deleted.sort_by(|a, b| a.path.cmp(&b.path));
        info.cluster_count = ((self.src.len() - self.vol) / 4096).min(u32::MAX as u64) as u32;
        info.allocated = used;
        Ok(info)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn crc32c_check_value() {
        // Check value of CRC-32C (Castagnoli) for "123456789".
        assert_eq!(super::crc32c(b"123456789"), 0xE306_9283);
    }
}
