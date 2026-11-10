//! Pass 2: frame-level carving. Finds H.264/H.265 NAL units directly on the
//! medium, regardless of file system or container, groups them into streams
//! and re-wraps each stream as MP4 without changing frame bytes.
//!
//! Framings:
//! * length-prefixed (4-byte big-endian size before each NAL), as stored in
//!   MP4/MOV `mdat` (ISO/IEC 14496-15);
//! * Annex B start codes (00 00 01), as used in raw streams, AVI and many
//!   proprietary recorder formats (ITU-T H.264 / H.265 Annex B).
//!
//! Recorders interleave audio between single video frames, so a stream is
//! rarely a long unbroken chain of NAL units. Detection therefore has two
//! levels:
//! * an *anchor* starts a stream: a chain of NAL units that is very unlikely
//!   by chance (3+ units, parameter sets + keyframe, or a keyframe whose slice
//!   header matches a known parameter set);
//! * a *continuation* (even a single NAL unit) is accepted only near the end
//!   of the current stream and only if its slice header is consistent with
//!   the stream (PPS id, frame_num / POC progression, size).

use crate::codec::{self, Codec, NalInfo, PpsInfo, SpsInfo};
use crate::io::Source;
use crate::mp4::{self, Mp4Writer, VideoConfig};
use crate::report::{self, Recovered};
use anyhow::Result;
use std::io::Write;
use std::path::{Path, PathBuf};

const MAX_NAL: u64 = 8 << 20;
const GENERIC_MAX_NALS: usize = 4096;
const GENERIC_MAX_BYTES: u64 = 64 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    LengthPrefixed,
    AnnexB,
}

#[derive(Debug, Clone, Copy)]
struct NalRef {
    off: u64, // NAL header position
    len: u32,
    info: NalInfo,
}

struct Window<'a> {
    src: &'a Source,
    base: u64,
    buf: Vec<u8>,
    /// Small reads outside the window (probes far ahead or behind) go here, so
    /// they do not throw away the large window.
    small: Vec<u8>,
}

impl<'a> Window<'a> {
    fn new(src: &'a Source) -> Self {
        Self {
            src,
            base: 0,
            buf: vec![],
            small: vec![],
        }
    }
    /// Returns up to `n` bytes at `off` (fewer at the end of the source).
    fn get(&mut self, off: u64, n: usize) -> Result<&[u8]> {
        let have = off >= self.base && off + n as u64 <= self.base + self.buf.len() as u64;
        if !have && n <= 64 << 10 {
            self.small.resize(n, 0);
            let got = self.src.read_at(off, &mut self.small)?;
            self.small.truncate(got);
            return Ok(&self.small);
        }
        if !have {
            if debug() {
                eprintln!("refetch @{off:#x} n={n} base={:#x}", self.base);
            }
            let size = n.max(16 << 20);
            self.buf.resize(size, 0);
            let got = self.src.read_at(off, &mut self.buf)?;
            self.buf.truncate(got);
            self.base = off;
        }
        let s = (off - self.base) as usize;
        let e = (s + n).min(self.buf.len());
        Ok(&self.buf[s.min(e)..e])
    }
}

/// Where a deleted directory entry pointed, used to label carved streams.
#[derive(Debug, Clone)]
pub struct Hint {
    pub extents: Vec<(u64, u64)>,
    pub name: String,
    pub modified: Option<String>,
    pub created: Option<String>,
}

pub struct Options {
    pub codecs: Vec<Codec>,
    pub fps: Option<f64>,
    pub max_gap: u64,
    pub min_frames: usize,
    pub keep_leading: bool,
}

/// Decoding context used to validate continuation NAL units.
#[derive(Debug, Clone, Default)]
struct Ctx {
    sps: Option<SpsInfo>,
    pps: Vec<PpsInfo>,
    prev_fnum: Option<u32>,
    prev_poc: Option<u32>,
    max_len: u32,
}

impl Ctx {
    fn from_config(c: Codec, cfg: &VideoConfig) -> Self {
        let mut x = Ctx {
            sps: cfg.sps_info(),
            ..Default::default()
        };
        x.pps = cfg
            .pps
            .iter()
            .filter_map(|p| codec::parse_pps(c, p))
            .collect();
        x
    }

    fn learn(&mut self, c: Codec, info: &NalInfo, nal: &[u8]) {
        match (c, info.nal_type) {
            (Codec::H264, 7) | (Codec::H265, 33) => {
                if let Some(s) = codec::parse_sps(c, nal) {
                    self.sps = Some(s);
                }
            }
            (Codec::H264, 8) | (Codec::H265, 34) => {
                if let Some(p) = codec::parse_pps(c, nal) {
                    self.pps.retain(|q| q.id != p.id);
                    self.pps.push(p);
                }
            }
            _ => {}
        }
    }

    /// Upper bound for one coded slice: the size of the uncompressed picture.
    fn frame_limit(&self) -> u32 {
        match &self.sps {
            Some(s) => {
                let samples =
                    s.width as u64 * s.height as u64 * if s.chroma_format_idc == 0 { 2 } else { 3 }
                        / 2;
                (samples * s.bit_depth_luma.max(8) as u64 / 8 + 4096).min(u32::MAX as u64) as u32
            }
            None => 0,
        }
    }

    fn head(&self, c: Codec, nal: &[u8]) -> Option<codec::SliceHead> {
        let id = codec::slice_head(c, nal, None, None)?.pps_id;
        let pps = self.pps.iter().find(|p| p.id == id);
        if !self.pps.is_empty() && pps.is_none() {
            return None;
        }
        codec::slice_head(c, nal, self.sps.as_ref(), pps)
    }

    /// Checks a VCL NAL against the stream and, if consistent, records it.
    fn accept_vcl(&mut self, c: Codec, info: &NalInfo, nal: &[u8], len: u32) -> bool {
        // Without parameter sets a slice header cannot be checked for
        // consistency, and random data passes the context-free checks far
        // too often.
        if self.sps.is_none() || self.pps.is_empty() {
            return false;
        }
        if len > self.frame_limit() {
            return false;
        }
        let Some(h) = self.head(c, nal) else {
            return false;
        };
        match c {
            Codec::H264 => {
                if let (Some(f), Some(s)) = (h.frame_num, self.sps.as_ref()) {
                    let max = 1u32 << s.log2_max_frame_num;
                    if info.is_keyframe {
                        if f != 0 {
                            return false;
                        }
                    } else if let Some(p) = self.prev_fnum
                        && f != p
                        && f != (p + 1) % max
                    {
                        return false;
                    }
                    let is_ref = nal[0] & 0x60 != 0;
                    if is_ref {
                        self.prev_fnum = Some(f);
                    }
                }
            }
            Codec::H265 => {
                if let (Some(poc), Some(s)) = (h.poc_lsb, self.sps.as_ref()) {
                    let max = 1u32 << s.log2_max_poc_lsb;
                    if !info.is_keyframe
                        && let Some(p) = self.prev_poc
                    {
                        let d = (poc + max - p) % max;
                        if d == 0 || (d > 32 && d < max - 32) {
                            return false;
                        }
                    }
                    self.prev_poc = Some(poc);
                }
            }
        }
        self.max_len = self.max_len.max(len);
        true
    }
}

struct Stream {
    codec: Codec,
    framing: Framing,
    nals: Vec<NalRef>,
    start: u64,
    end: u64,
    pieces: usize,
    ctx: Ctx,
    config: Option<VideoConfig>,
    /// Anchors that are not self-evidently real stay unconfirmed until a few
    /// consistent frames follow.
    confirmed: bool,
    vcl_since_anchor: usize,
}

fn debug() -> bool {
    std::env::var_os("VIDEOREC_DEBUG").is_some()
}

pub struct Carver<'a> {
    opts: &'a Options,
    configs: &'a [VideoConfig],
    hints: &'a [Hint],
    out_dir: PathBuf,
    scan: Window<'a>,
    follow: Window<'a>,
    cur: Option<Stream>,
    /// Stream set aside while an unconfirmed anchor is being checked.
    parked: Option<Stream>,
    pub results: Vec<Recovered>,
    pub discarded_small: usize,
    /// NAL chains that looked like video but had no parameter sets to decode
    /// them with (in-band, on the medium, or via --reference).
    pub chains_without_params: usize,
    no_params: bool,
    next_id: usize,
}

/// Validation mode for following a chain of NAL units.
enum Mode<'c> {
    Generic,
    InStream(&'c mut Ctx),
}

impl<'a> Carver<'a> {
    pub fn new(
        src: &'a Source,
        opts: &'a Options,
        configs: &'a [VideoConfig],
        hints: &'a [Hint],
        out_dir: &Path,
        first_id: usize,
    ) -> Self {
        Self {
            opts,
            configs,
            hints,
            out_dir: out_dir.to_path_buf(),
            scan: Window::new(src),
            follow: Window::new(src),
            cur: None,
            parked: None,
            results: vec![],
            discarded_small: 0,
            chains_without_params: 0,
            no_params: false,
            next_id: first_id,
        }
    }

    /// Scans one region. Streams may continue across regions if the gap is small.
    pub fn scan_region(&mut self, start: u64, end: u64) -> Result<()> {
        const CHUNK: usize = 4 << 20;
        let mut p = start;
        'outer: loop {
            crate::progress::at(p);
            if crate::progress::stopped() {
                break;
            }
            if p >= end {
                // An anchor that was never confirmed may have hidden real data
                // inside its (false) NAL unit: rescan from just after it.
                match self.reject_unconfirmed() {
                    Some(r) if r < end => {
                        p = r;
                        continue;
                    }
                    _ => break,
                }
            }
            if let Some(r) = self.timeout(p) {
                p = r;
                continue;
            }
            let want = CHUNK.min((end - p) as usize);
            let w = self.scan.get(p, want + 8)?.to_vec();
            let lim = want.min(w.len().saturating_sub(7));
            if lim == 0 {
                p = end;
                continue;
            }
            let mut i = 0;
            while i < lim {
                let x = &w[i..];
                let annexb = x[0] == 0 && x[1] == 0 && x[2] == 1;
                let lp = x[0] == 0 && x[1] < 0x80 && !(x[1] == 0 && x[2] == 0 && x[3] < 2);
                let hdr = if annexb { &x[3..] } else { &x[4..] };
                if (annexb || lp)
                    && self
                        .opts
                        .codecs
                        .iter()
                        .any(|&c| codec::classify(c, hdr).is_some())
                {
                    let at = p + i as u64;
                    if let Some(r) = self.timeout(at) {
                        p = r;
                        continue 'outer;
                    }
                    let framing = if annexb {
                        Framing::AnnexB
                    } else {
                        Framing::LengthPrefixed
                    };
                    if let Some(next) = self.try_at(at, end, framing)? {
                        p = next;
                        continue 'outer;
                    }
                }
                i += 1;
            }
            p += lim as u64;
        }
        Ok(())
    }

    /// Drops an unconfirmed stream, restores the parked one and returns the
    /// position to rescan from.
    fn reject_unconfirmed(&mut self) -> Option<u64> {
        if self.cur.as_ref().is_some_and(|s| !s.confirmed) {
            let s = self.cur.take().unwrap();
            self.cur = self.parked.take();
            if debug() {
                eprintln!("reject anchor @{:#x}", s.start);
            }
            return Some(s.start + 1);
        }
        None
    }

    fn timeout(&mut self, at: u64) -> Option<u64> {
        match &self.cur {
            Some(s) if !s.confirmed && at > s.end + self.opts.max_gap => self.reject_unconfirmed(),
            _ => None,
        }
    }

    /// Tries continuation of the current stream, then a new anchor, at `at`.
    fn try_at(&mut self, at: u64, end: u64, framing: Framing) -> Result<Option<u64>> {
        // Continuation.
        if let Some(mut cur) = self.cur.take() {
            let near = at >= cur.end && at - cur.end <= self.opts.max_gap && cur.framing == framing;
            let mut res = None;
            if near && !self.boundary_between(cur.end, at)? {
                let c = cur.codec;
                let (nals, run_end) =
                    self.follow(c, framing, at, end, Mode::InStream(&mut cur.ctx))?;
                if !nals.is_empty() {
                    if debug() {
                        eprintln!(
                            "cont @{at:#x} n={} t={} len={}",
                            nals.len(),
                            nals[0].info.nal_type,
                            nals[0].len
                        );
                    }
                    if at > cur.end {
                        cur.pieces += 1;
                    }
                    cur.vcl_since_anchor += nals.iter().filter(|n| n.info.is_vcl).count();
                    cur.nals.extend(nals);
                    cur.end = run_end;
                    res = Some(run_end);
                }
            }
            let newly_confirmed = !cur.confirmed && cur.vcl_since_anchor >= 3;
            if newly_confirmed {
                cur.confirmed = true;
            }
            self.cur = Some(cur);
            if newly_confirmed && let Some(pk) = self.parked.take() {
                self.write_stream(pk)?;
            }
            if res.is_some() {
                return Ok(res);
            }
        }
        // New anchor.
        for ci in 0..self.opts.codecs.len() {
            let c = self.opts.codecs[ci];
            let (mut nals, mut run_end) = self.follow(c, framing, at, end, Mode::Generic)?;
            if nals.is_empty() {
                continue;
            }
            let n0 = nals.len();
            if debug() && n0 > 2 {
                eprintln!("try @{at:#x} n={n0} end={run_end:#x}");
            }
            self.no_params = false;
            let got = self.anchor(c, &mut nals)?;
            if nals.len() < n0
                && let Some(l) = nals.last()
            {
                run_end = l.off + l.len as u64;
            }
            if got.is_none() && self.no_params && n0 >= 3 {
                // Every later unit of this chain would be rejected for the
                // same reason (no parameter sets anywhere in it): skip the
                // chain instead of retrying from each of its start codes.
                return Ok(Some(run_end));
            }
            if let Some((ctx, config, strong)) = got {
                if debug() {
                    eprintln!(
                        "anchor @{at:#x} {:?} n={} t={} strong={strong}",
                        c,
                        nals.len(),
                        nals[0].info.nal_type
                    );
                }
                // A second anchor before the first was confirmed: the first
                // was probably false; rescan from just after it.
                if let Some(r) = self.reject_unconfirmed() {
                    return Ok(Some(r));
                }
                let new = Stream {
                    codec: c,
                    framing,
                    start: at,
                    end: run_end,
                    pieces: 1,
                    ctx,
                    config,
                    nals,
                    confirmed: strong,
                    vcl_since_anchor: 0,
                };
                if strong {
                    self.flush()?;
                } else if let Some(old) = self.cur.take() {
                    self.parked = Some(old);
                }
                self.cur = Some(new);
                return Ok(Some(run_end));
            }
        }
        Ok(None)
    }

    /// True if a container header lies between two positions: the stream
    /// belongs to a different file from there on.
    fn boundary_between(&mut self, a: u64, b: u64) -> Result<bool> {
        if b <= a {
            return Ok(false);
        }
        let gap = self.follow.get(a, (b - a) as usize)?;
        Ok([b"ftyp".as_slice(), b"moov", b"RIFF"]
            .iter()
            .any(|s| memchr::memmem::find(gap, s).is_some()))
    }

    /// Decides whether a generically valid chain is a stream start. Returns the
    /// validation context and the parameter-set source to use.
    fn anchor(
        &mut self,
        c: Codec,
        nals: &mut Vec<NalRef>,
    ) -> Result<Option<(Ctx, Option<VideoConfig>, bool)>> {
        let mut ctx = Ctx::default();
        let mut has_sps = false;
        let mut has_pps = false;
        for n in nals.iter() {
            if n.info.is_param_set {
                let b = self.follow.get(n.off, n.len as usize)?.to_vec();
                ctx.learn(c, &n.info, &b);
                has_sps |= matches!((c, n.info.nal_type), (Codec::H264, 7) | (Codec::H265, 33));
                has_pps |= matches!((c, n.info.nal_type), (Codec::H264, 8) | (Codec::H265, 34));
            }
        }
        let first_vcl = nals.iter().position(|n| n.info.is_vcl);
        let strong_chain = nals.len() >= 3
            || (nals.len() >= 2 && !nals[0].info.is_vcl && nals.iter().any(|n| n.info.is_keyframe));
        if has_sps && has_pps {
            if !strong_chain {
                return Ok(None);
            }
            let mut x = ctx;
            let limit = x.frame_limit();
            if let Some(k) = nals.iter().position(|n| n.info.is_vcl && n.len > limit) {
                // An oversized "frame" ends the real stream (it is data that
                // merely follows it): keep the part before it.
                nals.truncate(k);
                if nals.is_empty() {
                    return Ok(None);
                }
                return self.anchor(c, nals);
            }
            self.prime(c, &mut x, nals);
            return Ok(Some((x, None, true)));
        }
        // Out-of-band parameter sets: pick the candidate that matches the slices.
        let heads: Vec<Vec<u8>> = nals
            .iter()
            .filter(|n| n.info.is_vcl)
            .take(8)
            .map(|n| {
                self.follow
                    .get(n.off, (n.len as usize).min(64))
                    .map(|b| b.to_vec())
            })
            .collect::<Result<_>>()?;
        let best = self.best_config(c, &heads);
        if best.is_none() {
            if strong_chain && nals.len() >= 3 {
                self.chains_without_params += 1;
                self.no_params = true;
            }
            return Ok(None);
        }
        if !strong_chain {
            // Lone keyframe: accept only with a matching candidate and an
            // I-slice header that starts the picture with frame_num 0.
            let Some(i) = first_vcl else { return Ok(None) };
            let n = &nals[i];
            let Some((cfg, score)) = &best else {
                return Ok(None);
            };
            let x = Ctx::from_config(c, cfg);
            if !n.info.is_keyframe
                || !n.info.first_slice
                || n.len < 1024
                || n.len > x.frame_limit()
                || *score < 1
            {
                return Ok(None);
            }
            let Some(h) = x.head(c, &heads[0]) else {
                return Ok(None);
            };
            let intra = match c {
                Codec::H264 => {
                    matches!(h.slice_type, 2 | 7 | 4 | 9) && h.frame_num.unwrap_or(0) == 0
                }
                Codec::H265 => h.slice_type == 2 || h.poc_lsb.is_none(),
            };
            if !intra {
                return Ok(None);
            }
        }
        let (mut x, cfg) = match best {
            Some((cfg, _)) => (Ctx::from_config(c, &cfg), Some(cfg)),
            None => (ctx, None),
        };
        let limit = x.frame_limit();
        if let Some(k) = nals.iter().position(|n| n.info.is_vcl && n.len > limit) {
            nals.truncate(k);
            if nals.is_empty() {
                return Ok(None);
            }
            return self.anchor(c, nals);
        }
        self.prime(c, &mut x, nals);
        Ok(Some((x, cfg, strong_chain && nals.len() >= 3)))
    }

    /// Feeds the anchor's NAL units through the context (sets frame_num/POC state).
    fn prime(&mut self, c: Codec, x: &mut Ctx, nals: &[NalRef]) {
        for n in nals {
            if let Ok(b) = self
                .follow
                .get(n.off, (n.len as usize).min(64))
                .map(|b| b.to_vec())
            {
                if n.info.is_vcl {
                    let _ = x.accept_vcl(c, &n.info, &b, n.len);
                }
                x.max_len = x.max_len.max(n.len);
            }
        }
    }

    /// Scores candidates by slice-header consistency; returns the best one.
    fn best_config(&self, c: Codec, heads: &[Vec<u8>]) -> Option<(VideoConfig, i64)> {
        let mut best: Option<(VideoConfig, i64)> = None;
        for cf in self.configs.iter().filter(|x| x.codec == c) {
            let mut x = Ctx::from_config(c, cf);
            let mut score = 0i64;
            for h in heads {
                let Some(info) = codec::classify(c, h) else {
                    continue;
                };
                if x.accept_vcl(c, &info, h, 0) {
                    score += 1
                } else {
                    score -= 2
                }
            }
            if best.as_ref().is_none_or(|(_, b)| score > *b) {
                best = Some((cf.clone(), score));
            }
        }
        best
    }

    /// Follows a chain of NAL units from `at`. In-stream mode validates each
    /// unit against the stream context.
    fn follow(
        &mut self,
        c: Codec,
        framing: Framing,
        at: u64,
        end: u64,
        mut mode: Mode,
    ) -> Result<(Vec<NalRef>, u64)> {
        let mut nals = vec![];
        let mut run_end = at;
        let finder = memchr::memmem::Finder::new(&[0u8, 0, 1]);
        let mut q = at;
        let generic = matches!(mode, Mode::Generic);
        loop {
            // Deciding whether a chain starts a stream needs only its first
            // few thousand units; the rest is followed as a continuation.
            // Without this cap every rejected candidate walked to the end of
            // all contiguous NAL-like data (hundreds of MB), many times over.
            if generic && (nals.len() >= GENERIC_MAX_NALS || q - at >= GENERIC_MAX_BYTES) {
                break;
            }
            // Locate the next NAL: (header offset, length, next position).
            let (off, len, next) = match framing {
                Framing::LengthPrefixed => {
                    if q + 6 > end {
                        break;
                    }
                    let b = self.follow.get(q, 4)?;
                    if b.len() < 4 {
                        break;
                    }
                    let len = u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as u64;
                    if !(2..=MAX_NAL).contains(&len) || q + 4 + len > end {
                        break;
                    }
                    (q + 4, len, q + 4 + len)
                }
                Framing::AnnexB => {
                    let h = if q == at { q + 3 } else { q };
                    if h + 2 > end {
                        break;
                    }
                    let span = ((end - h).min(MAX_NAL + 3)) as usize;
                    let b = self.follow.get(h, span)?;
                    if codec::classify(c, b).is_none() {
                        break;
                    }
                    let Some(sc) = finder.find(&b[1..]).map(|x| x + 1) else {
                        break;
                    };
                    let mut e = sc;
                    while e > 1 && b[e - 1] == 0 {
                        e -= 1;
                    }
                    let e = trim_container_junk(&b[..e]);
                    (h, e as u64, h + sc as u64 + 3)
                }
            };
            let head = self.follow.get(off, (len as usize).min(64))?.to_vec();
            let Some(info) = codec::classify(c, &head) else {
                break;
            };
            let ok = match &mut mode {
                Mode::Generic => !info.is_vcl || codec::slice_head(c, &head, None, None).is_some(),
                Mode::InStream(ctx) => {
                    if info.is_vcl {
                        ctx.accept_vcl(c, &info, &head, len as u32)
                    } else if info.is_param_set {
                        if len <= 4096 {
                            let b = self.follow.get(off, len as usize)?.to_vec();
                            ctx.learn(c, &info, &b);
                            true
                        } else {
                            false
                        }
                    } else {
                        len <= nonvcl_max(c, info.nal_type)
                    }
                }
            };
            if !ok {
                break;
            }
            nals.push(NalRef {
                off,
                len: len as u32,
                info,
            });
            run_end = match framing {
                Framing::LengthPrefixed => next,
                Framing::AnnexB => next - 3,
            };
            q = next;
        }
        // A chain counts only up to its last frame: lone SEI/AUD-like units
        // are too easy to match by chance.
        match nals.iter().rposition(|n| n.info.is_vcl) {
            Some(k) => {
                if k + 1 < nals.len() {
                    let last = nals[k];
                    nals.truncate(k + 1);
                    run_end = match framing {
                        Framing::LengthPrefixed => last.off + last.len as u64,
                        Framing::AnnexB => last.off + last.len as u64,
                    };
                }
            }
            None => nals.clear(),
        }
        Ok((nals, run_end))
    }

    /// Writes out the parked and current streams.
    pub fn flush(&mut self) -> Result<()> {
        if let Some(p) = self.parked.take() {
            self.write_stream(p)?;
        }
        if let Some(s) = self.cur.take()
            && s.confirmed
        {
            self.write_stream(s)?;
        }
        Ok(())
    }

    fn write_stream(&mut self, s: Stream) -> Result<()> {
        let c = s.codec;

        // Group NAL units into access units (frames).
        let mut aus: Vec<Vec<NalRef>> = vec![];
        let mut cur: Vec<NalRef> = vec![];
        let mut has_vcl = false;
        for n in &s.nals {
            let starts = if n.info.is_vcl {
                n.info.first_slice && has_vcl
            } else {
                has_vcl && starts_au(c, n.info.nal_type)
            };
            if starts {
                aus.push(std::mem::take(&mut cur));
                has_vcl = false;
            }
            has_vcl |= n.info.is_vcl;
            cur.push(*n);
        }
        if has_vcl {
            aus.push(cur);
        }
        let total = aus.len();
        let mut dropped = 0;
        if !self.opts.keep_leading {
            let k = aus
                .iter()
                .position(|au| au.iter().any(|n| n.info.is_keyframe))
                .unwrap_or(aus.len());
            aus.drain(..k);
            dropped = k;
        }
        if aus.len() < self.opts.min_frames {
            self.discarded_small += 1;
            return Ok(());
        }

        let mut notes = vec![];
        if dropped > 0 {
            notes.push(format!(
                "{dropped} of {total} frames before the first keyframe dropped"
            ));
        }
        // Parameter sets: in-band first, else the candidate chosen at the anchor
        // (re-checked against more frames).
        let (vps, sps, pps) = self.in_band(&s)?;
        let config = if !sps.is_empty() && !pps.is_empty() {
            Some(VideoConfig {
                codec: c,
                length_size: 4,
                vps,
                sps,
                pps,
                fps: None,
                origin: "in-band".into(),
            })
        } else {
            let heads: Vec<Vec<u8>> = aus
                .iter()
                .take(90)
                .filter_map(|au| au.iter().find(|n| n.info.is_vcl))
                .map(|n| {
                    self.follow
                        .get(n.off, (n.len as usize).min(64))
                        .map(|b| b.to_vec())
                })
                .collect::<Result<_>>()?;
            match self.best_config(c, &heads) {
                Some((cf, _)) if self.configs.iter().filter(|x| x.codec == c).count() > 1 => {
                    let mut cf = cf;
                    cf.origin = format!("{} (chosen by slice-header match)", cf.origin);
                    Some(cf)
                }
                Some((cf, _)) => Some(cf),
                None => s.config.clone(),
            }
        };
        if let Some(cf) = &config
            && s.framing == Framing::LengthPrefixed
            && cf.length_size != 4
        {
            notes.push(format!(
                "config uses {}-byte NAL lengths; stream was carved assuming 4",
                cf.length_size
            ));
        }

        let id = self.next_id;
        self.next_id += 1;
        let hint = self
            .hints
            .iter()
            .find(|h| h.extents.iter().any(|e| e.0 <= s.start && s.start < e.1))
            .cloned();
        let stem = match &hint {
            Some(h) => {
                let base = h.name.rsplit('/').next().unwrap_or("");
                let stem = base.rsplit_once('.').map(|(a, _)| a).unwrap_or(base);
                let clean = sanitize(stem);
                if clean.chars().any(|c| c.is_alphanumeric()) {
                    format!("stream_{id:05}_{clean}")
                } else {
                    format!("stream_{id:05}_{:012x}", s.start)
                }
            }
            None => format!("stream_{id:05}_{:012x}", s.start),
        };
        let keyframes = aus
            .iter()
            .filter(|au| au.iter().any(|n| n.info.is_keyframe))
            .count();
        let mut rec = Recovered {
            id,
            kind: "carved_stream".into(),
            source_start: s.start,
            source_end: s.end,
            codec: Some(c.name().into()),
            frames: Some(aus.len()),
            keyframes: Some(keyframes),
            pieces: Some(s.pieces),
            name_hint: hint.as_ref().map(|h| h.name.clone()),
            modified: hint.as_ref().and_then(|h| h.modified.clone()),
            created: hint.as_ref().and_then(|h| h.created.clone()),
            ..Default::default()
        };

        let entry = config.as_ref().and_then(mp4::sample_entry);
        let path;
        if let (Some(cf), Some(entry)) = (&config, entry) {
            path = self.out_dir.join(format!("{stem}.mp4"));
            let fps = self.opts.fps.or(cf.fps).unwrap_or(30.0);
            let mut w = Mp4Writer::create(&path)?;
            for au in &aus {
                let mut data = vec![];
                for n in au {
                    let b = self.follow.get(n.off, n.len as usize)?;
                    data.extend_from_slice(&(b.len() as u32).to_be_bytes());
                    data.extend_from_slice(b);
                }
                w.add_sample(&data, au.iter().any(|n| n.info.is_keyframe))?;
            }
            w.finish(&entry, fps)?;
            let info = cf.sps_info();
            rec.width = info.as_ref().map(|i| i.width);
            rec.height = info.as_ref().map(|i| i.height);
            rec.duration_s = Some(aus.len() as f64 / fps);
            rec.config_source = Some(cf.origin.clone());
            rec.status = if dropped > 0 {
                "partial".into()
            } else {
                "ok".into()
            };
            if self.opts.fps.is_none() && cf.fps.is_none() {
                notes.push("frame rate unknown, 30 fps assumed".into());
            }
            notes.push("video only (audio not carved)".into());
        } else {
            // No parameter sets anywhere: keep the raw elementary stream.
            path = self.out_dir.join(format!("{stem}.{}", c.name()));
            let mut f = std::io::BufWriter::new(std::fs::File::create(&path)?);
            for au in &aus {
                for n in au {
                    let b = self.follow.get(n.off, n.len as usize)?;
                    f.write_all(&[0, 0, 0, 1])?;
                    f.write_all(b)?;
                }
            }
            f.flush()?;
            rec.status = "needs_parameter_sets".into();
            notes.push(
                "no SPS/PPS found; supply --reference with a healthy file from the same device"
                    .into(),
            );
        }
        rec.output = path.file_name().unwrap().to_string_lossy().into();
        rec.size = std::fs::metadata(&path)?.len();
        rec.sha256 = report::sha256_file(&path)?;
        rec.notes = notes.join("; ");
        self.results.push(rec);
        Ok(())
    }

    fn in_band(&mut self, s: &Stream) -> Result<(Vec<Vec<u8>>, Vec<Vec<u8>>, Vec<Vec<u8>>)> {
        let (mut v, mut sp, mut pp) = (vec![], vec![], vec![]);
        for n in &s.nals {
            if !n.info.is_param_set {
                continue;
            }
            let b = self.follow.get(n.off, n.len as usize)?.to_vec();
            let slot = match (s.codec, n.info.nal_type) {
                (Codec::H264, 7) | (Codec::H265, 33) => &mut sp,
                (Codec::H264, 8) | (Codec::H265, 34) => &mut pp,
                (Codec::H265, 32) => &mut v,
                _ => continue,
            };
            if !slot.contains(&b) {
                slot.push(b);
            }
            if !sp.is_empty() && !pp.is_empty() && (s.codec == Codec::H264 || !v.is_empty()) {
                break;
            }
        }
        if !sp.is_empty() && codec::parse_sps(s.codec, &sp[0]).is_none() {
            sp.clear();
        }
        Ok((v, sp, pp))
    }
}

/// Largest plausible size of a non-VCL unit accepted as continuation.
fn nonvcl_max(c: Codec, t: u8) -> u64 {
    match (c, t) {
        (Codec::H264, 9) | (Codec::H265, 35) => 8, // access unit delimiter
        (Codec::H264, 10 | 11) | (Codec::H265, 36 | 37) => 4, // end of sequence/stream
        _ => 64 << 10,                             // SEI, filler
    }
}

fn starts_au(c: Codec, t: u8) -> bool {
    match c {
        Codec::H264 => matches!(t, 6..=9),
        Codec::H265 => matches!(t, 32..=35 | 39),
    }
}

/// When a NAL unit is delimited only by the next start code, any container
/// data in between (AVI chunk headers, interleaved audio chunks) ends up
/// inside it. Cuts at the first AVI chunk header: `##dc`, `##db`, `##wb`,
/// `##tx`, `ix##`, `idx1`, `JUNK`, `LIST` followed by a plausible size.
fn trim_container_junk(b: &[u8]) -> usize {
    let mut i = 1;
    while i + 8 <= b.len() {
        let id = &b[i..i + 4];
        let tagged = (id[0].is_ascii_digit()
            && id[1].is_ascii_digit()
            && matches!(&id[2..4], b"dc" | b"db" | b"wb" | b"tx"))
            || (&id[..2] == b"ix" && id[2].is_ascii_digit() && id[3].is_ascii_digit())
            || id == b"idx1"
            || id == b"JUNK"
            || id == b"LIST";
        if tagged {
            let size = u32::from_le_bytes(b[i + 4..i + 8].try_into().unwrap());
            if size < 16 << 20 {
                return i;
            }
        }
        i += 1;
    }
    b.len()
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trims_avi_chunk_header() {
        let mut v = vec![0x65, 1, 2, 3];
        v.extend_from_slice(b"01wb");
        v.extend_from_slice(&100u32.to_le_bytes());
        v.extend_from_slice(&[9; 20]);
        assert_eq!(trim_container_junk(&v), 4);
    }
}
