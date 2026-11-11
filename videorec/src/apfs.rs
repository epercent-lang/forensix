//! APFS metadata: live files, allocated space, and deleted files whose
//! records survive in older file-system tree nodes.
//!
//! Layout as documented in Apple's "Apple File System Reference":
//! container super block (NXSB) and its checkpoint area, object maps,
//! volume super blocks (APSB), file-system trees with inode and file
//! extent records, space manager bitmaps. Only reading is implemented, for
//! unencrypted volumes on a single device; snapshots, Fusion drives,
//! compressed files and sparse files are not handled.
//!
//! APFS never changes a tree node in place: every change writes a new node
//! and frees the old one, which stays on disk until the block is reused.
//! Free blocks are therefore searched for file-system tree leaves with a
//! correct checksum; a file that no volume has any more is rebuilt from the
//! inode and extent records in them when every block is still free. Old
//! nodes do not say which volume they belonged to, so with several volumes
//! a file is treated as deleted only if no volume has an inode with its
//! number. Free blocks are read only on containers up to 64 GiB (set
//! VIDEOREC_FS_SCAN_GB to change).

use crate::fs::{DeletedFile, FsInfo};
use crate::io::{Source, Volume, le16, le32, le64};
use crate::ranges::Ranges;
use crate::xfs::unix_time;
use anyhow::{Result, bail};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};

pub fn is_apfs(b0: &[u8]) -> bool {
    b0.len() >= 44
        && &b0[32..36] == b"NXSB"
        && le32(b0, 36).is_power_of_two()
        && (4096..=65536).contains(&le32(b0, 36))
}

/// Fletcher-64 as used for APFS object headers: true when the stored
/// checksum matches the rest of the block.
fn checksum_ok(b: &[u8]) -> bool {
    let (mut s1, mut s2) = (0u64, 0u64);
    for w in b[8..].chunks_exact(4) {
        s1 = (s1 + u32::from_le_bytes(w.try_into().unwrap()) as u64) % 0xFFFF_FFFF;
        s2 = (s2 + s1) % 0xFFFF_FFFF;
    }
    let c1 = 0xFFFF_FFFF - ((s1 + s2) % 0xFFFF_FFFF);
    let c2 = 0xFFFF_FFFF - ((s1 + c1) % 0xFFFF_FFFF);
    le64(b, 0) == (c2 << 32 | c1) && le64(b, 0) != 0
}

/// Key and value byte ranges of a B-tree node's entries.
fn entries(b: &[u8]) -> Vec<(usize, usize, usize, usize)> {
    let bs = b.len();
    let flags = le16(b, 32);
    let n = le32(b, 36) as usize;
    let toc = 56 + le16(b, 40) as usize;
    let keys = toc + le16(b, 42) as usize;
    let vend = bs - if flags & 1 != 0 { 40 } else { 0 };
    let fixed = flags & 4 != 0;
    let leaf = flags & 2 != 0;
    let mut out = Vec::with_capacity(n.min(1024));
    for i in 0..n {
        let (ko, kl, vo, vl) = if fixed {
            let e = toc + 4 * i;
            if e + 4 > keys.min(bs) {
                break;
            }
            (le16(b, e) as usize, 16, le16(b, e + 2) as usize, if leaf { 16 } else { 8 })
        } else {
            let e = toc + 8 * i;
            if e + 8 > keys.min(bs) {
                break;
            }
            (
                le16(b, e) as usize,
                le16(b, e + 2) as usize,
                le16(b, e + 4) as usize,
                le16(b, e + 6) as usize,
            )
        };
        let k = keys + ko;
        if k + kl > bs || vo > vend || vo < vl.min(vo) || vend - vo + vl > bs || vo == 0xFFFF {
            continue;
        }
        out.push((k, kl, vend - vo, vl));
    }
    out
}

#[derive(Clone, Default)]
struct Ino {
    parent: u64,
    private: u64,
    mode: u16,
    size: u64,
    name: String,
    mtime: Option<String>,
    crtime: Option<String>,
    xid: u64,
}

/// Records collected from file-system tree leaves.
#[derive(Default)]
struct Recs {
    inodes: HashMap<u64, Ino>,
    /// Data stream id -> logical byte offset -> (physical block, bytes, xid).
    extents: HashMap<u64, BTreeMap<u64, (u64, u64, u64)>>,
}

pub struct Apfs<'a> {
    src: &'a Source,
    vol: u64,
    bs: u64,
    blocks: u64,
    sb: Vec<u8>,
    desc: (u64, u64),
    cache: RefCell<HashMap<u64, Vec<u8>>>,
}

impl<'a> Apfs<'a> {
    pub fn open(src: &'a Source, vol: &Volume) -> Result<Self> {
        let b0 = src.read_vec(vol.offset, 4096)?;
        if !is_apfs(&b0) {
            bail!("not an APFS container");
        }
        let bs = le32(&b0, 36) as u64;
        let mut me = Apfs {
            src,
            vol: vol.offset,
            bs,
            blocks: le64(&b0, 40),
            sb: src.read_vec(vol.offset, bs as usize)?,
            desc: (le64(&b0, 112), (le32(&b0, 104) & 0x7FFF_FFFF) as u64),
            cache: RefCell::new(HashMap::new()),
        };
        // The newest valid super block of the checkpoint descriptor area.
        if le32(&b0, 104) & 0x8000_0000 == 0 {
            for i in 0..me.desc.1.min(4096) {
                if let Ok(b) = me.block(me.desc.0 + i)
                    && is_apfs(&b)
                    && checksum_ok(&b)
                    && (le64(&b, 16) > le64(&me.sb, 16) || !checksum_ok(&me.sb))
                {
                    me.sb = b;
                }
            }
        }
        Ok(me)
    }

    fn block(&self, n: u64) -> Result<Vec<u8>> {
        if n >= self.blocks {
            bail!("block outside the container");
        }
        if let Some(b) = self.cache.borrow().get(&n) {
            return Ok(b.clone());
        }
        let b = self.src.read_vec(self.vol + n * self.bs, self.bs as usize)?;
        let mut c = self.cache.borrow_mut();
        if c.len() > 2048 {
            c.clear();
        }
        c.insert(n, b.clone());
        Ok(b)
    }

    /// Physical block of object `oid` (newest version up to `xid`) in the
    /// object map whose B-tree root is at block `root`.
    fn omap_get(&self, root: u64, oid: u64, xid: u64) -> Option<u64> {
        let mut at = root;
        for _ in 0..12 {
            let b = self.block(at).ok()?;
            let leaf = le16(&b, 32) & 2 != 0;
            let mut best = None;
            for (k, _, v, _) in entries(&b) {
                let key = (le64(&b, k), le64(&b, k + 8));
                if key <= (oid, xid) && best.is_none_or(|(bk, _)| key >= bk) {
                    best = Some((key, v));
                }
            }
            let (key, v) = best?;
            if leaf {
                return (key.0 == oid).then(|| le64(&b, v + 8));
            }
            at = le64(&b, v);
        }
        None
    }

    /// Block of an object map's tree root, from the map object's block.
    fn omap_root(&self, omap: u64) -> Option<u64> {
        let b = self.block(omap).ok()?;
        (le32(&b, 24) & 0xFFFF == 0xB).then(|| le64(&b, 48))
    }

    /// Records of one file-system tree leaf.
    fn leaf(&self, b: &[u8], out: &mut Recs) {
        let xid = le64(b, 16);
        for (k, kl, v, vl) in entries(b) {
            if kl < 8 {
                continue;
            }
            let id = le64(b, k) & ((1 << 60) - 1);
            match le64(b, k) >> 60 {
                3 if vl >= 92 => {
                    let val = &b[v..v + vl];
                    let mut ino = Ino {
                        parent: le64(val, 0),
                        private: le64(val, 8),
                        mode: le16(val, 80),
                        mtime: unix_time((le64(val, 24) / 1_000_000_000) as i64),
                        crtime: unix_time((le64(val, 16) / 1_000_000_000) as i64),
                        xid,
                        ..Default::default()
                    };
                    // Extended fields: name (4) and data stream (8).
                    if vl >= 96 {
                        let n = le16(val, 92) as usize;
                        let mut data = 96 + 4 * n;
                        for f in 0..n {
                            let h = 96 + 4 * f;
                            if h + 4 > vl {
                                break;
                            }
                            let size = le16(val, h + 2) as usize;
                            if data + size > vl {
                                break;
                            }
                            match val[h] {
                                4 => {
                                    let raw = &val[data..data + size];
                                    let raw =
                                        &raw[..raw.iter().position(|&c| c == 0).unwrap_or(size)];
                                    ino.name = String::from_utf8_lossy(raw).replace('/', "_");
                                }
                                8 if size >= 8 => ino.size = le64(val, data),
                                _ => {}
                            }
                            data += size.div_ceil(8) * 8;
                        }
                    }
                    let old = out.inodes.get(&id);
                    if old.is_none_or(|o| o.xid <= xid || o.size == 0) {
                        out.inodes.insert(id, ino);
                    }
                }
                8 if kl >= 16 && vl >= 16 => {
                    let len = le64(b, v) & 0x00FF_FFFF_FFFF_FFFF;
                    let phys = le64(b, v + 8);
                    let m = out.extents.entry(id).or_default();
                    let logical = le64(b, k + 8);
                    if m.get(&logical).is_none_or(|e| e.2 <= xid) {
                        m.insert(logical, (phys, len, xid));
                    }
                }
                _ => {}
            }
        }
    }

    fn walk(&self, omap: u64, oid: u64, xid: u64, depth: u32, out: &mut Recs) {
        let Some(at) = self.omap_get(omap, oid, xid) else {
            return;
        };
        let Ok(b) = self.block(at) else { return };
        if depth > 12 || le32(&b, 28) != 0xE {
            return;
        }
        if le16(&b, 32) & 2 != 0 {
            self.leaf(&b, out);
        } else {
            for (_, _, v, vl) in entries(&b) {
                if vl >= 8 {
                    self.walk(omap, le64(&b, v), xid, depth + 1, out);
                }
            }
        }
    }

    fn byte_extents(&self, r: &Recs, ino: &Ino) -> Option<Vec<(u64, u64)>> {
        let m = r.extents.get(&ino.private)?;
        let mut out: Vec<(u64, u64)> = vec![];
        let mut pos = 0u64;
        for (&logical, &(phys, len, _)) in m {
            if pos >= ino.size {
                break;
            }
            if logical != pos || phys == 0 || len == 0 || phys + len / self.bs > self.blocks {
                return None;
            }
            let l = len.min(ino.size - pos);
            let a = self.vol + phys * self.bs;
            match out.last_mut() {
                Some(x) if x.1 == a => x.1 += l,
                _ => out.push((a, a + l)),
            }
            pos += len;
        }
        (pos >= ino.size).then_some(out)
    }

    fn path_of(r: &Recs, id: u64) -> String {
        let mut parts = vec![];
        let (mut k, mut guard) = (id, 0);
        while k != 2 && k != 1 && guard < 256 {
            match r.inodes.get(&k) {
                Some(i) if !i.name.is_empty() => {
                    parts.push(i.name.clone());
                    k = i.parent;
                }
                _ => {
                    parts.push(format!("apfs_inode_{k}"));
                    break;
                }
            }
            guard += 1;
        }
        parts.reverse();
        format!("/{}", parts.join("/"))
    }

    /// Allocated space from the space manager's chunk bitmaps; `None` when
    /// the space manager cannot be read.
    fn allocation(&self) -> Option<Ranges> {
        let (xid, sm_oid) = (le64(&self.sb, 16), le64(&self.sb, 152));
        // The space manager is listed in this checkpoint's map blocks.
        let mut sm_at = None;
        let mut ephemeral: HashMap<u64, u64> = HashMap::new();
        for i in 0..self.desc.1.min(4096) {
            let Ok(b) = self.block(self.desc.0 + i) else { continue };
            if le32(&b, 24) & 0xFFFF != 0xC || le64(&b, 16) != xid {
                continue;
            }
            for k in 0..(le32(&b, 36) as usize).min((b.len() - 40) / 40) {
                let e = 40 + 40 * k;
                if le64(&b, e + 24) == sm_oid {
                    sm_at = Some(le64(&b, e + 32));
                }
                ephemeral.insert(le64(&b, e + 24), le64(&b, e + 32));
            }
        }
        let sm = self.block(sm_at?).ok()?;
        if le32(&sm, 24) & 0xFFFF != 5 {
            return None;
        }
        let per_chunk = le32(&sm, 36) as u64;
        let (cibs, cabs) = (le32(&sm, 64) as usize, le32(&sm, 68) as usize);
        let addr = le32(&sm, 80) as usize;
        let list = |b: &[u8], at: usize, n: usize| -> Vec<u64> {
            (0..n)
                .filter(|k| at + 8 * k + 8 <= b.len())
                .map(|k| le64(b, at + 8 * k))
                .collect()
        };
        let cib_addrs: Vec<u64> = if cabs == 0 {
            list(&sm, addr, cibs)
        } else {
            let mut v = vec![];
            for cab in list(&sm, addr, cabs) {
                let b = self.block(cab).ok()?;
                v.extend(list(&b, 40, le32(&b, 36) as usize));
            }
            v
        };
        let mut r = Ranges::new();
        for cib in cib_addrs {
            let b = self.block(cib).ok()?;
            if le32(&b, 24) & 0xFFFF != 7 {
                return None;
            }
            for k in 0..(le32(&b, 36) as usize).min((b.len() - 40) / 32) {
                let e = 40 + 32 * k;
                let (first, count, free, map) = (
                    le64(&b, e + 8),
                    le32(&b, e + 16) as u64,
                    le32(&b, e + 20) as u64,
                    le64(&b, e + 24),
                );
                if map == 0 || free == count {
                    continue;
                }
                let bits = self.block(map).ok()?;
                let mut run: Option<u64> = None;
                for i in 0..count.min(per_chunk).min(self.bs * 8) {
                    let used = bits[(i / 8) as usize] & (1 << (i % 8)) != 0;
                    match (used, run) {
                        (true, None) => run = Some(i),
                        (false, Some(s)) => {
                            r.add(self.vol + (first + s) * self.bs, self.vol + (first + i) * self.bs);
                            run = None;
                        }
                        _ => {}
                    }
                }
                if let Some(s) = run {
                    r.add(
                        self.vol + (first + s) * self.bs,
                        self.vol + (first + count.min(per_chunk)) * self.bs,
                    );
                }
            }
        }
        r.add(self.vol, self.vol + self.bs);
        r.normalize();
        // Blocks waiting in the main free queue are released already as
        // far as the files are concerned; the bitmap follows later.
        let whole = (self.vol, self.vol + self.blocks * self.bs);
        let mut free = Ranges::new();
        for (a, b) in r.gaps(whole.0, whole.1) {
            free.add(a, b);
        }
        let mut stack: Vec<u64> = ephemeral.get(&le64(&sm, 200 + 40 + 8)).copied().into_iter().collect();
        let mut seen = HashSet::new();
        while let Some(at) = stack.pop() {
            if !seen.insert(at) || seen.len() > 1 << 16 {
                continue;
            }
            let Ok(b) = self.block(at) else { continue };
            let flags = le16(&b, 32);
            if le32(&b, 28) != 9 || flags & 4 == 0 {
                continue;
            }
            let toc = 56 + le16(&b, 40) as usize;
            let keys = toc + le16(&b, 42) as usize;
            let vend = b.len() - if flags & 1 != 0 { 40 } else { 0 };
            for i in 0..le32(&b, 36) as usize {
                let e = toc + 4 * i;
                if e + 4 > keys.min(b.len()) {
                    break;
                }
                let (k, vo) = (keys + le16(&b, e) as usize, le16(&b, e + 2) as usize);
                if k + 16 > b.len() {
                    continue;
                }
                // A missing value means one block.
                let val = (vo != 0xFFFF && vo >= 8 && vo <= vend).then(|| le64(&b, vend - vo));
                if flags & 2 == 0 {
                    stack.extend(val.and_then(|oid| ephemeral.get(&oid).copied()));
                } else {
                    let (first, n) = (le64(&b, k + 8), val.unwrap_or(1));
                    if first < self.blocks && n <= self.blocks - first {
                        free.add(self.vol + first * self.bs, self.vol + (first + n) * self.bs);
                    }
                }
            }
        }
        free.normalize();
        let mut r = Ranges::new();
        for (a, b) in free.gaps(whole.0, whole.1) {
            r.add(a, b);
        }
        r.normalize();
        Some(r)
    }

    pub fn analyze(&self) -> Result<FsInfo> {
        let mut info = FsInfo {
            cluster_size: self.bs,
            cluster_count: self.blocks.min(u32::MAX as u64) as u32,
            ..Default::default()
        };
        let xid = le64(&self.sb, 16);
        let Some(nx_omap) = self.omap_root(le64(&self.sb, 160)) else {
            bail!("APFS container object map not found");
        };
        // Volumes.
        let mut vols: Vec<(String, Recs)> = vec![];
        for k in 0..(le32(&self.sb, 180) as usize).min(100) {
            let oid = le64(&self.sb, 184 + 8 * k);
            if oid == 0 {
                continue;
            }
            let Some(vsb) = self.omap_get(nx_omap, oid, xid).and_then(|a| self.block(a).ok())
            else {
                continue;
            };
            if &vsb[32..36] != b"APSB" || le64(&vsb, 264) & 1 == 0 {
                // Not a volume, or encrypted.
                continue;
            }
            let raw = &vsb[704..960];
            let name = String::from_utf8_lossy(&raw[..raw.iter().position(|&c| c == 0).unwrap_or(0)])
                .replace('/', "_");
            let mut recs = Recs::default();
            if let Some(omap) = self.omap_root(le64(&vsb, 128)) {
                self.walk(omap, le64(&vsb, 136), xid, 0, &mut recs);
            }
            vols.push((name, recs));
        }
        let many = vols.len() > 1;
        let mut live_ids: HashSet<u64> = HashSet::new();
        let mut live_starts: HashSet<u64> = HashSet::new();
        for (name, recs) in &vols {
            for (&id, ino) in &recs.inodes {
                live_ids.insert(id);
                if ino.mode & 0xF000 != 0x8000 || ino.size == 0 {
                    continue;
                }
                if let Some(extents) = self.byte_extents(recs, ino) {
                    live_starts.insert(extents[0].0);
                    let p = Self::path_of(recs, id);
                    info.live.push(DeletedFile {
                        path: if many { format!("/{name}{p}") } else { p },
                        size: ino.size,
                        first_cluster: 0,
                        extents,
                        method: "inode and extent records",
                        modified: ino.mtime.clone(),
                        created: ino.crtime.clone(),
                    });
                }
            }
        }
        info.live.sort_by(|a, b| a.path.cmp(&b.path));
        info.live_files = info.live.len();

        let Some(allocated) = self.allocation() else {
            // Without the bitmaps nothing can be called free: the whole
            // container counts as allocated and no deleted file is listed.
            info.allocated.add(self.vol, self.vol + self.blocks * self.bs);
            return Ok(info);
        };
        let limit_gb = std::env::var("VIDEOREC_FS_SCAN_GB")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(64);
        let mut old = Recs::default();
        if self.blocks * self.bs <= limit_gb << 30 {
            let step = (4u64 << 20) / self.bs * self.bs;
            for (a, b) in allocated.gaps(self.vol, self.vol + self.blocks * self.bs) {
                let mut p = a;
                while p < b {
                    let n = (b - p).min(step) as usize;
                    if let Ok(buf) = self.src.read_vec(p, n) {
                        for blk in buf.chunks_exact(self.bs as usize) {
                            let kind = le32(blk, 24) & 0xFFFF;
                            if (kind == 2 || kind == 3)
                                && le32(blk, 28) == 0xE
                                && le16(blk, 32) & 2 != 0
                                && checksum_ok(blk)
                            {
                                self.leaf(blk, &mut old);
                            }
                        }
                    }
                    p += n as u64;
                }
            }
        }
        // Names of folders that still exist come from the live trees.
        for (_, recs) in &vols {
            for (id, ino) in &recs.inodes {
                if ino.mode & 0xF000 == 0x4000 {
                    old.inodes.entry(*id).or_insert_with(|| ino.clone());
                }
            }
        }
        for (&id, ino) in &old.inodes {
            if live_ids.contains(&id) || ino.mode & 0xF000 != 0x8000 || ino.size == 0 {
                continue;
            }
            let Some(extents) = self.byte_extents(&old, ino) else {
                continue;
            };
            if live_starts.contains(&extents[0].0)
                || extents
                    .iter()
                    .any(|&(a, b)| allocated.gaps(a, b) != vec![(a, b)])
            {
                continue;
            }
            info.deleted.push(DeletedFile {
                path: Self::path_of(&old, id),
                size: ino.size,
                first_cluster: 0,
                extents,
                method: "APFS: records in an older tree node (free block); blocks still free",
                modified: ino.mtime.clone(),
                created: ino.crtime.clone(),
            });
        }
        info.deleted.sort_by(|a, b| a.path.cmp(&b.path));
        info.allocated = allocated;
        Ok(info)
    }
}
