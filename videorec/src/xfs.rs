//! XFS metadata: live files, free space, and deleted files whose block list
//! is still in the log.
//!
//! Layout as documented in "XFS Algorithms & Data Structures" (the XFS
//! on-disk format specification shipped with xfsprogs/xfs-documentation) and
//! the kernel's xfs_format.h / xfs_log_format.h: super block, AGF free-space
//! B+tree, inodes and their extent lists, directory blocks, log records.
//! Only reading is implemented. Everything on disk is big-endian, except the
//! inode core inside log records, which is in the byte order of the machine
//! that wrote it (little-endian is assumed here).
//!
//! Deleting a file on XFS removes its extent list from the inode. The log
//! often still holds an older image of the inode with the list; that image
//! gives the data back, as long as the blocks have not been reused. Names
//! come from older images of the parent directory in the log, or from the
//! entry left behind in a directory block.

use crate::fs::{DeletedFile, FsInfo};
use crate::io::{Source, Volume, be32, be64};
use crate::ranges::Ranges;
use anyhow::{Result, bail};
use std::collections::{HashMap, HashSet};

const NULL32: u32 = u32::MAX;

pub fn is_xfs(sb: &[u8]) -> bool {
    sb.len() >= 512 && &sb[..4] == b"XFSB"
}

fn be16(b: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([b[o], b[o + 1]])
}

pub struct Xfs<'a> {
    src: &'a Source,
    vol: u64,
    bs: u64,
    agblocks: u64,
    agcount: u64,
    agblklog: u32,
    inopblog: u32,
    inode_size: usize,
    sect: u64,
    root: u64,
    v5: bool,
    ftype: bool,
    log_start: u64,
    log_blocks: u64,
}

struct Inode {
    mode: u16,
    format: u8,
    size: u64,
    nextents: u32,
    mtime: Option<String>,
    crtime: Option<String>,
    /// Data fork bytes.
    fork: Vec<u8>,
}

/// One extent: file block offset, absolute byte offset, block count.
type Ext = (u64, u64, u64);

fn unix_time(secs: i64) -> Option<String> {
    (946_684_800..4_102_444_800)
        .contains(&secs)
        .then(|| crate::jdr::format_time(secs * 1000)[..19].to_string())
}

/// Bigtime: nanoseconds since 1901-12-13T20:45:52Z (xfs_format.h).
fn bigtime(v: u64) -> Option<String> {
    unix_time((v / 1_000_000_000) as i64 - (1i64 << 31))
}

impl<'a> Xfs<'a> {
    pub fn open(src: &'a Source, vol: &Volume) -> Result<Self> {
        let sb = src.read_vec(vol.offset, 512)?;
        if !is_xfs(&sb) {
            bail!("not an XFS super block");
        }
        let bs = be32(&sb, 4) as u64;
        let version = be16(&sb, 100) & 0xF;
        let v5 = version == 5;
        let x = Xfs {
            src,
            vol: vol.offset,
            bs,
            agblocks: be32(&sb, 84) as u64,
            agcount: be32(&sb, 88) as u64,
            agblklog: sb[124] as u32,
            inopblog: sb[123] as u32,
            inode_size: be16(&sb, 104) as usize,
            sect: be16(&sb, 102) as u64,
            root: be64(&sb, 56),
            v5,
            ftype: if v5 {
                be32(&sb, 216) & 1 != 0
            } else {
                be32(&sb, 200) & 0x200 != 0
            },
            log_start: be64(&sb, 48),
            log_blocks: be32(&sb, 96) as u64,
        };
        if !(512..=65536).contains(&bs)
            || !bs.is_power_of_two()
            || !(4..=5).contains(&version)
            || x.agblocks == 0
            || x.agcount == 0
            || !(256..=2048).contains(&x.inode_size)
            || sb[192] != 0
        {
            // sb[192]: directory blocks larger than file system blocks.
            bail!("XFS variant not supported (version {version}, block size {bs})");
        }
        Ok(x)
    }

    fn fsb(&self, fsb: u64) -> u64 {
        let ag = fsb >> self.agblklog;
        let bno = fsb & ((1 << self.agblklog) - 1);
        self.vol + (ag * self.agblocks + bno) * self.bs
    }

    fn block(&self, abs: u64) -> Result<Vec<u8>> {
        let b = self.src.read_vec(abs, self.bs as usize)?;
        if b.len() < self.bs as usize {
            bail!("XFS block beyond the end of the source");
        }
        Ok(b)
    }

    fn inode_at(&self, ino: u64) -> Option<u64> {
        let bits = self.agblklog + self.inopblog;
        let (ag, agino) = (ino >> bits, ino & ((1 << bits) - 1));
        if ag >= self.agcount {
            return None;
        }
        Some(
            self.vol
                + (ag * self.agblocks + (agino >> self.inopblog)) * self.bs
                + (agino & ((1 << self.inopblog) - 1)) * self.inode_size as u64,
        )
    }

    fn inode(&self, ino: u64) -> Result<Option<Inode>> {
        let Some(at) = self.inode_at(ino) else {
            return Ok(None);
        };
        let b = self.src.read_vec(at, self.inode_size)?;
        if b.len() < self.inode_size || &b[..2] != b"IN" {
            return Ok(None);
        }
        let v3 = b[4] >= 3;
        let big = v3 && be64(&b, 120) & 8 != 0;
        let time = |o: usize| {
            if big {
                bigtime(be64(&b, o))
            } else {
                unix_time(be32(&b, o) as i32 as i64)
            }
        };
        let start = if v3 { 176 } else { 100 };
        let end = if b[82] != 0 {
            (start + b[82] as usize * 8).min(self.inode_size)
        } else {
            self.inode_size
        };
        Ok(Some(Inode {
            mode: be16(&b, 2),
            format: b[5],
            size: be64(&b, 56),
            nextents: be32(&b, 76),
            mtime: time(40),
            crtime: if v3 { time(144) } else { None },
            fork: b[start..end].to_vec(),
        }))
    }

    fn decode(&self, r: &[u8]) -> Ext {
        let (l0, l1) = (be64(r, 0), be64(r, 8));
        let off = (l0 & !(1 << 63)) >> 9;
        let fsb = ((l0 & 0x1FF) << 43) | (l1 >> 21);
        (off, self.fsb(fsb), l1 & 0x1F_FFFF)
    }

    /// Extent list of an inode's data fork (extent or B+tree format).
    fn extents(&self, i: &Inode) -> Result<Option<Vec<Ext>>> {
        let mut out = vec![];
        match i.format {
            2 => {
                let n = (i.nextents as usize).min(i.fork.len() / 16);
                for k in 0..n {
                    out.push(self.decode(&i.fork[k * 16..]));
                }
            }
            3 => {
                if i.fork.len() < 20 {
                    return Ok(None);
                }
                let level = be16(&i.fork, 0);
                let maxrecs = (i.fork.len() - 4) / 16;
                let mut blk = self.fsb(be64(&i.fork, 4 + maxrecs * 8));
                let hdr = if self.v5 { 72 } else { 24 };
                let mut depth = level;
                // Down the leftmost path, then along the leaves.
                loop {
                    let b = self.block(blk)?;
                    if !matches!(&b[..4], b"BMAP" | b"BMA3") {
                        return Ok(None);
                    }
                    let lvl = be16(&b, 4);
                    if lvl == 0 {
                        break;
                    }
                    let maxrecs = (self.bs as usize - hdr) / 16;
                    blk = self.fsb(be64(&b, hdr + maxrecs * 8));
                    depth = depth.saturating_sub(1);
                    if depth == 0 && lvl > 1 {
                        return Ok(None);
                    }
                }
                let mut guard = 0;
                loop {
                    let b = self.block(blk)?;
                    if !matches!(&b[..4], b"BMAP" | b"BMA3") || be16(&b, 4) != 0 {
                        return Ok(None);
                    }
                    let n = (be16(&b, 6) as usize).min((self.bs as usize - hdr) / 16);
                    for k in 0..n {
                        out.push(self.decode(&b[hdr + k * 16..]));
                    }
                    let right = be64(&b, 16);
                    guard += 1;
                    if right == u64::MAX || guard > 1_000_000 {
                        break;
                    }
                    blk = self.fsb(right);
                }
            }
            _ => return Ok(None),
        }
        Ok(Some(out))
    }

    /// Absolute byte extents of the first `size` bytes; `None` for a file
    /// with holes.
    fn byte_extents(&self, ext: &[Ext], size: u64) -> Option<Vec<(u64, u64)>> {
        let mut out: Vec<(u64, u64)> = vec![];
        let (mut next, mut left) = (0u64, size);
        for &(off, abs, n) in ext {
            if left == 0 {
                break;
            }
            if off != next || n == 0 || abs + n * self.bs > self.src.len() {
                return None;
            }
            let l = left.min(n * self.bs);
            match out.last_mut() {
                Some(x) if x.1 == abs => x.1 += l,
                _ => out.push((abs, abs + l)),
            }
            left -= l;
            next += n;
        }
        (left == 0 && !out.is_empty()).then_some(out)
    }

    /// Bytes in use: each allocation group minus its free extents.
    fn allocation(&self) -> Result<Ranges> {
        let mut used = Ranges::new();
        let hdr = if self.v5 { 56 } else { 16 };
        for ag in 0..self.agcount {
            let base = self.vol + ag * self.agblocks * self.bs;
            let agf = self.src.read_vec(base + self.sect, 64)?;
            if agf.len() < 64 || &agf[..4] != b"XAGF" {
                bail!("XFS allocation group {ag}: AGF header not found");
            }
            let len = be32(&agf, 12) as u64;
            let mut free: Vec<(u64, u64)> = vec![];
            let mut blk = be32(&agf, 16);
            let mut guard = 0;
            // Down to the leftmost leaf of the by-block-number tree.
            loop {
                let b = self.block(base + blk as u64 * self.bs)?;
                if !matches!(&b[..4], b"ABTB" | b"AB3B") {
                    bail!("XFS allocation group {ag}: free-space tree block not found");
                }
                if be16(&b, 4) == 0 {
                    break;
                }
                let maxrecs = (self.bs as usize - hdr) / 12;
                blk = be32(&b, hdr + maxrecs * 8);
                guard += 1;
                if guard > 16 {
                    bail!("XFS free-space tree too deep");
                }
            }
            while blk != NULL32 && guard < 1_000_000 {
                let b = self.block(base + blk as u64 * self.bs)?;
                if !matches!(&b[..4], b"ABTB" | b"AB3B") || be16(&b, 4) != 0 {
                    bail!("XFS allocation group {ag}: free-space leaf not found");
                }
                let n = (be16(&b, 6) as usize).min((self.bs as usize - hdr) / 8);
                for k in 0..n {
                    let (s, c) = (
                        be32(&b, hdr + k * 8) as u64,
                        be32(&b, hdr + k * 8 + 4) as u64,
                    );
                    free.push((s, s + c));
                }
                blk = be32(&b, 12);
                guard += 1;
            }
            free.sort_unstable();
            let mut p = 0;
            for (s, e) in free {
                if s > p {
                    used.add(base + p * self.bs, base + s.min(len) * self.bs);
                }
                p = p.max(e);
            }
            if p < len {
                used.add(base + p * self.bs, base + len * self.bs);
            }
        }
        used.normalize();
        Ok(used)
    }

    /// Entries of a short-form directory (stored in the inode).
    fn shortform(&self, f: &[u8]) -> Vec<(u64, String)> {
        let mut out = vec![];
        if f.len() < 6 {
            return out;
        }
        let (count, i8) = (f[0] as usize, f[1] > 0);
        let mut p = if i8 { 10 } else { 6 };
        for _ in 0..count.max(f[1] as usize) {
            let Some(&nl) = f.get(p) else { break };
            let nl = nl as usize;
            let after = p + 3 + nl + self.ftype as usize;
            let need = if i8 { 8 } else { 4 };
            if nl == 0 || after + need > f.len() {
                break;
            }
            let ino = if i8 {
                be64(f, after)
            } else {
                be32(f, after) as u64
            };
            out.push((
                ino,
                String::from_utf8_lossy(&f[p + 3..p + 3 + nl]).into_owned(),
            ));
            p = after + need;
        }
        out
    }

    /// Entries of a directory data block: (inode, name, removed). A removed
    /// entry keeps its name and the low 32 bits of its inode number.
    fn dir_block(&self, b: &[u8]) -> Vec<(u64, String, bool)> {
        let mut out = vec![];
        let (hdr, single) = match &b[..4] {
            b"XD2B" => (16, true),
            b"XDB3" => (64, true),
            b"XD2D" => (16, false),
            b"XDD3" => (64, false),
            _ => return out,
        };
        let mut end = b.len();
        if single {
            let count = be32(b, b.len() - 8) as usize;
            end = match (b.len() - 8).checked_sub(count * 8) {
                Some(e) if e >= hdr => e,
                _ => return out,
            };
        }
        let entry = |q: usize, limit: usize, low_only: bool| -> Option<(u64, String, usize)> {
            let nl = *b.get(q + 8)? as usize;
            let size = (8 + 1 + nl + self.ftype as usize + 2).div_ceil(8) * 8;
            if nl == 0 || q + size > limit {
                return None;
            }
            let name = &b[q + 9..q + 9 + nl];
            if name.iter().any(|&c| c == 0 || c == b'/') {
                return None;
            }
            let ino = if low_only {
                be32(b, q + 4) as u64
            } else {
                be64(b, q)
            };
            Some((ino, String::from_utf8_lossy(name).into_owned(), size))
        };
        let mut p = hdr;
        while p + 8 <= end {
            if be16(b, p) == 0xFFFF {
                let len = be16(b, p + 2) as usize;
                if len < 8 || len % 8 != 0 || p + len > end {
                    break;
                }
                // What the freed space held before.
                let mut q = p;
                while q + 16 <= p + len {
                    let low = be16(b, q) == 0xFFFF;
                    match entry(q, p + len, low) {
                        Some((ino, name, size)) if ino != 0 => {
                            out.push((ino, name, true));
                            q += size;
                        }
                        _ => break,
                    }
                }
                p += len;
            } else {
                let Some((ino, name, size)) = entry(p, end, false) else {
                    break;
                };
                out.push((ino, name, false));
                p += size;
            }
        }
        out
    }

    /// Log records with the first word of each 512-byte block restored
    /// (the log stores the cycle number there). Oldest position first.
    fn log_records(&self) -> Result<Vec<Vec<u8>>> {
        let mut out = vec![];
        if self.log_start == 0 || self.log_blocks == 0 {
            return Ok(out); // external log
        }
        let base = self.fsb(self.log_start);
        let len = (self.log_blocks * self.bs).min(2 << 30);
        let log = self.src.read_vec(base, len as usize)?;
        let mut p = 0usize;
        while p + 512 <= log.len() {
            if be32(&log, p) != 0xFEED_BABE {
                p += 512;
                continue;
            }
            let h_len = be32(&log, p + 12) as usize;
            let h_size = be32(&log, p + 320) as usize;
            let hdrs = if be32(&log, p + 8) & 2 != 0 && h_size > 32768 {
                h_size.div_ceil(32768)
            } else {
                1
            };
            let start = p + hdrs * 512;
            let bbs = h_len.div_ceil(512);
            if h_len == 0 || start + bbs * 512 > log.len() {
                p += 512;
                continue;
            }
            let mut rec = log[start..start + bbs * 512].to_vec();
            for i in 0..bbs {
                // Cycle data: 64 words in the header, then 64 per extra header.
                let (h, k) = (i / 64, i % 64);
                let at = if h == 0 {
                    p + 44 + k * 4
                } else {
                    p + h * 512 + 4 + k * 4
                };
                if h < hdrs {
                    rec[i * 512..i * 512 + 4].copy_from_slice(&log[at..at + 4]);
                }
            }
            rec.truncate(h_len);
            out.push(rec);
            p = start + bbs * 512;
        }
        Ok(out)
    }

    pub fn analyze(&self) -> Result<FsInfo> {
        let allocated = self.allocation()?;
        let mut info = FsInfo {
            cluster_size: self.bs,
            cluster_count: (self.agblocks * self.agcount).min(u32::MAX as u64) as u32,
            ..Default::default()
        };
        // Walk the directory tree.
        let mut dir_paths: HashMap<u64, String> = HashMap::new();
        let mut live: HashSet<u64> = HashSet::new();
        // Names of removed entries, by the low 32 bits of the inode number.
        let mut removed: HashMap<u64, String> = HashMap::new();
        let mut stack = vec![(self.root, String::new())];
        while let Some((dino, dpath)) = stack.pop() {
            if dir_paths.contains_key(&dino) || dir_paths.len() > 2_000_000 {
                continue;
            }
            dir_paths.insert(dino, dpath.clone());
            let Some(d) = self.inode(dino)? else { continue };
            if d.mode & 0xF000 != 0x4000 {
                continue;
            }
            let mut entries: Vec<(u64, String, bool)> = vec![];
            if d.format == 1 {
                entries.extend(
                    self.shortform(&d.fork)
                        .into_iter()
                        .map(|(i, n)| (i, n, false)),
                );
            } else if let Some(ext) = self.extents(&d)? {
                for (off, abs, n) in ext {
                    // Blocks from 32 GiB on hold the directory's index.
                    if off * self.bs >= 32 << 30 {
                        continue;
                    }
                    for k in 0..n.min(1 << 16) {
                        if let Ok(b) = self.block(abs + k * self.bs) {
                            entries.extend(self.dir_block(&b));
                        }
                    }
                }
            }
            for (ino, name, gone) in entries {
                if name == "." || name == ".." {
                    continue;
                }
                let path = format!("{dpath}/{name}");
                if gone {
                    removed.entry(ino).or_insert(path);
                    continue;
                }
                let Some(i) = self.inode(ino)? else { continue };
                match i.mode & 0xF000 {
                    0x4000 => stack.push((ino, path)),
                    0x8000 if i.size > 0 => {
                        live.insert(ino);
                        if let Some(e) = self
                            .extents(&i)?
                            .and_then(|x| self.byte_extents(&x, i.size))
                        {
                            info.live.push(DeletedFile {
                                path,
                                size: i.size,
                                first_cluster: 0,
                                extents: e,
                                method: "inode extents",
                                modified: i.mtime.clone(),
                                created: i.crtime.clone(),
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        info.live_files = info.live.len();

        // Deleted files: older inode images in the log. An inode log item is
        // an inode core (written in the machine's byte order, magic "IN")
        // followed by a 12-byte operation header and the data fork.
        struct Old {
            size: u64,
            ext: Vec<Ext>,
            mtime: Option<String>,
            crtime: Option<String>,
        }
        let mut old: HashMap<u64, Old> = HashMap::new();
        let mut old_names: HashMap<u64, (u64, String)> = HashMap::new(); // ino -> (dir, name)
        let le32 = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let le64 = |b: &[u8], o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        for rec in self.log_records()? {
            let mut p = 0;
            while let Some(k) = memchr::memmem::find(&rec[p..], b"NI") {
                let at = p + k;
                p = at + 2;
                let Some(c) = rec.get(at..at + 176 + 12) else {
                    break;
                };
                let mode = u16::from_le_bytes([c[2], c[3]]);
                if c[4] != 3 || !(1..=3).contains(&c[5]) {
                    continue;
                }
                let (ino, size, nextents) = (le64(c, 152), le64(c, 56), le32(c, 76) as usize);
                let flen = be32(c, 176 + 4) as usize;
                if self.inode_at(ino).is_none() || flen > self.inode_size {
                    continue;
                }
                let Some(fork) = rec.get(at + 188..at + 188 + flen) else {
                    continue;
                };
                let big = le64(c, 120) & 8 != 0;
                let time = |o: usize| {
                    if big {
                        bigtime(le64(c, o))
                    } else {
                        unix_time(le32(c, o) as i32 as i64)
                    }
                };
                if mode & 0xF000 == 0x4000 && c[5] == 1 {
                    for (child, name) in self.shortform(fork) {
                        old_names.insert(child, (ino, name));
                    }
                } else if mode & 0xF000 == 0x8000
                    && c[5] == 2
                    && size > 0
                    && nextents > 0
                    && flen == nextents * 16
                {
                    let ext = (0..nextents)
                        .map(|k| self.decode(&fork[k * 16..]))
                        .collect();
                    // Later records are newer: the last image with data wins.
                    old.insert(
                        ino,
                        Old {
                            size,
                            ext,
                            mtime: time(40),
                            crtime: time(144),
                        },
                    );
                }
            }
        }
        for (ino, o) in old {
            if live.contains(&ino) {
                continue;
            }
            // Still a file with data (renamed, or reached another way)?
            if self.inode(ino)?.is_some_and(|i| i.mode != 0 && i.size > 0) {
                continue;
            }
            let Some(extents) = self.byte_extents(&o.ext, o.size) else {
                continue;
            };
            // Blocks in use again belong to something else now.
            if extents
                .iter()
                .any(|&(a, b)| allocated.gaps(a, b) != vec![(a, b)])
            {
                continue;
            }
            let path = match old_names.get(&ino) {
                Some((dir, name)) => match dir_paths.get(dir) {
                    Some(d) => format!("{d}/{name}"),
                    None => format!("/{name}"),
                },
                None => removed
                    .get(&(ino & 0xFFFF_FFFF))
                    .cloned()
                    .unwrap_or_else(|| format!("/xfs_inode_{ino}")),
            };
            info.deleted.push(DeletedFile {
                path,
                size: o.size,
                first_cluster: 0,
                extents,
                method: "XFS log: older image of the inode (extent list); blocks still free",
                modified: o.mtime,
                created: o.crtime,
            });
        }
        info.deleted.sort_by(|a, b| a.path.cmp(&b.path));
        info.allocated = allocated;
        Ok(info)
    }
}
