//! AVI recordings of dashcams with more than one camera: fragments are split
//! by the stream number of every chunk instead of carving the H.264 inside,
//! which would mix the cameras.
//!
//! Layout measured on a user's 64 GB card (FAT32, setup.ini "Model =
//! VG-9000"), on live files:
//!
//! - RIFF AVI with five streams: `00dc` front and `01dc` rear camera (H.264
//!   1920x1080, Annex B, key frames start with SPS and PPS), `02wb` mono
//!   signed 16-bit PCM at 16000 Hz in 8192-byte chunks, `03st` GPS (8 bytes)
//!   and `04st` sensor (12 bytes) once per frame;
//! - chunks of odd size are **not** followed by a pad byte, unlike the AVI
//!   specification (Microsoft, "AVI RIFF File Reference"); both forms are
//!   accepted here;
//! - the header's JUNK chunk holds the file name, e.g.
//!   `REC2_20261027_191041.avi`, which is the only record of the time.
//!
//! A run is a chain of chunks that follow each other without interruption.
//! Runs that start right after an AVI header get their time from the name in
//! it; other runs have no known time.

use crate::codec::{self, Codec};
use crate::io::Source;
use crate::jdr::{Chunk, Kind, Origin, days_from_civil};
use crate::ranges::Ranges;
use anyhow::Result;
use std::collections::{BTreeMap, HashMap};

const MAX_CHUNK: usize = 4 << 20;
/// Runs with fewer chunks are ignored.
const MIN_CHUNKS: usize = 6;
/// Times before this (2000-01-01) are placeholders of runs without a name.
pub const KNOWN_TIME_MS: i64 = 946_684_800_000;

#[derive(Debug, Default, serde::Serialize)]
pub struct Stats {
    pub runs: usize,
    pub runs_with_file_name: usize,
    pub frames: usize,
    pub key_frames: usize,
    pub audio_chunks: usize,
    pub per_camera: BTreeMap<u32, usize>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tag {
    Video,
    Audio,
    Other,
}

/// Stream number and kind of a chunk header at the start of `b`, with the
/// payload length.
fn head(b: &[u8]) -> Option<(u8, Tag, usize)> {
    if b.len() < 8 || !b[0].is_ascii_digit() || !b[1].is_ascii_digit() {
        return None;
    }
    let tag = match &b[2..4] {
        b"dc" | b"db" => Tag::Video,
        b"wb" => Tag::Audio,
        b"st" | b"tx" => Tag::Other,
        _ => return None,
    };
    let n = u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize;
    if n > MAX_CHUNK {
        return None;
    }
    if tag == Tag::Video && n > 0 {
        let p = b.get(8..12)?;
        if !(p[..3] == [0, 0, 1] || p == [0, 0, 0, 1]) {
            return None;
        }
    }
    Some(((b[0] - b'0') * 10 + (b[1] - b'0'), tag, n))
}

/// Offset of the chunk after the one at `p` (length `n`): directly behind it,
/// or after a pad byte. `Err(true)` when an index (`idx1`) or the end of the
/// RIFF follows, `Err(false)` when nothing valid follows.
fn next(b: &[u8], p: usize, n: usize) -> std::result::Result<usize, bool> {
    let q = p + 8 + n;
    for q in [q, q + (n & 1)] {
        if head(b.get(q..).unwrap_or(&[])).is_some() {
            return Ok(q);
        }
        if b.get(q..q + 4).is_some_and(|t| t == b"idx1" || t == b"RIFF") {
            return Err(true);
        }
    }
    Err(false)
}

/// True when 1 MiB at one of `offsets` (starts of live video files) or at
/// evenly spread places of `regions` holds a chunk chain with at least two
/// video streams.
pub fn detect(src: &Source, offsets: &[u64], regions: &[(u64, u64)]) -> Result<bool> {
    let mut buf = vec![0u8; 1 << 20];
    let mut check = |off: u64, lim: u64| -> Result<bool> {
        let want = buf.len().min(lim.saturating_sub(off) as usize);
        let n = src.read_at(off, &mut buf[..want])?;
        Ok(two_cameras(&buf[..n]))
    };
    for &o in offsets.iter().take(16) {
        if check(o, src.len())? {
            return Ok(true);
        }
    }
    let total: u64 = regions.iter().map(|r| r.1 - r.0).sum();
    const N: u64 = 64;
    for i in 0..N {
        let mut t = total / N * i;
        for &(a, b) in regions {
            if t < b - a {
                if check(a + t, b)? {
                    return Ok(true);
                }
                break;
            }
            t -= b - a;
        }
    }
    Ok(false)
}

fn two_cameras(b: &[u8]) -> bool {
    let f = memchr::memmem::Finder::new(b"dc");
    let mut i = 2;
    while let Some(k) = f.find(&b[i.min(b.len())..]) {
        let mut p = i + k - 2;
        i += k + 1;
        let mut seen = [false; 100];
        let mut links = 0;
        while let Some((s, tag, n)) = head(&b[p..]) {
            if tag == Tag::Video {
                seen[s as usize] = true;
            }
            match next(b, p, n) {
                Ok(q) => p = q,
                Err(_) => break,
            }
            links += 1;
            if links >= 4 && seen.iter().filter(|&&x| x).count() >= 2 {
                return true;
            }
            if links > 64 {
                break;
            }
        }
    }
    false
}

/// Recording start (ms since 1970, recorder's clock) and frames per second
/// from an AVI header that ends at `movi` (offset of the "movi" word).
fn header_info(src: &Source, movi: u64) -> Result<(Option<i64>, Option<u32>)> {
    let back = movi.min(64 << 10);
    let h = src.read_vec(movi - back, back as usize)?;
    let Some(r) = memchr::memmem::rfind(&h, b"RIFF") else {
        return Ok((None, None));
    };
    let h = &h[r..];
    if h.get(8..12) != Some(b"AVI ") {
        return Ok((None, None));
    }
    let fps = memchr::memmem::find(h, b"avih")
        .and_then(|p| h.get(p + 8..p + 12))
        .map(|u| u32::from_le_bytes(u.try_into().unwrap()))
        .filter(|&us| (4_000..=1_000_000).contains(&us))
        .map(|us| ((1_000_000 + us / 2) / us).max(1));
    // `_YYYYMMDD_hhmmss.avi`
    let mut time = None;
    for p in memchr::memmem::find_iter(h, b".avi") {
        if p < 16 {
            continue;
        }
        let s = &h[p - 16..p];
        let digits = |x: &[u8]| -> Option<i64> {
            x.iter()
                .try_fold(0i64, |a, &c| c.is_ascii_digit().then(|| a * 10 + (c - b'0') as i64))
        };
        if s[0] != b'_' || s[9] != b'_' {
            continue;
        }
        let (Some(d), Some(t)) = (digits(&s[1..9]), digits(&s[10..16])) else {
            continue;
        };
        let (y, mo, da) = (d / 10000, d / 100 % 100, d % 100);
        let (hh, mi, ss) = (t / 10000, t / 100 % 100, t % 100);
        if (1990..2100).contains(&y)
            && (1..=12).contains(&mo)
            && (1..=31).contains(&da)
            && hh < 24
            && mi < 60
            && ss < 60
        {
            time = Some((days_from_civil(y, mo, da) * 86400 + hh * 3600 + mi * 60 + ss) * 1000);
            break;
        }
    }
    Ok((time, fps))
}

struct Run {
    base_ms: i64,
    fps: u32,
    /// Frames written so far per stream.
    count: HashMap<u8, u32>,
    start: u64,
    first_chunk: usize,
    chunks: usize,
}

/// Finds chunk chains in `regions`. Accepted runs are added to `covered`.
pub fn scan(
    src: &Source,
    regions: &[(u64, u64)],
    covered: &mut Ranges,
    stats: &mut Stats,
) -> Result<Vec<Chunk>> {
    const BUF: usize = 32 << 20;
    let finder = memchr::memmem::Finder::new(b"dc");
    let mut buf = vec![0u8; BUF];
    let mut out: Vec<Chunk> = vec![];
    // Picture size per stream, from the last SPS seen.
    let mut size: HashMap<u8, (u32, u32)> = HashMap::new();
    let mut unknown_runs = 0i64;
    let mut done = 0u64;
    for &(start, end) in regions {
        crate::progress::region(start, done);
        done += end - start;
        let mut base = start;
        let mut run: Option<Run> = None;
        // Position of the next chunk of the open run, or where to search.
        let mut at = start;
        while base < end {
            crate::progress::at(at);
            if crate::progress::stopped() {
                break;
            }
            let want = BUF.min((end - base) as usize);
            let n = src.read_at(base, &mut buf[..want])?;
            if n < 16 {
                break;
            }
            let last = base + n as u64 >= end;
            // Chunks starting before `lim` are complete in the buffer,
            // together with the header of the chunk after them.
            let lim = if last { n } else { n - MAX_CHUNK - 32 };
            let b = &buf[..n];
            let mut i = (at - base) as usize;
            loop {
                if run.is_none() {
                    // Look for a video chunk that is followed by another chunk.
                    let mut found = None;
                    while i + 2 < lim {
                        let Some(k) = finder.find(&b[i + 2..lim.max(i + 2)]) else {
                            i = lim;
                            break;
                        };
                        let p = i + k;
                        i = p + 1;
                        if let Some((_, Tag::Video, len)) = head(&b[p..])
                            && len > 0
                            && next(b, p, len).is_ok()
                        {
                            found = Some(p);
                            break;
                        }
                    }
                    let Some(p) = found else {
                        at = base + i.min(n) as u64;
                        break;
                    };
                    // An AVI header right before it? ("movi" within the few
                    // small chunks that may precede the first frame.)
                    let off = base + p as u64;
                    let lb = src.read_vec(off.saturating_sub(256), off.min(256) as usize)?;
                    let (time, fps) = match memchr::memmem::rfind(&lb, b"movi") {
                        Some(m) => header_info(src, off - lb.len() as u64 + m as u64)?,
                        None => (None, None),
                    };
                    let base_ms = time.unwrap_or_else(|| {
                        unknown_runs += 1;
                        unknown_runs * 3_600_000
                    });
                    run = Some(Run {
                        base_ms,
                        fps: fps.unwrap_or(30),
                        count: HashMap::new(),
                        start: off,
                        first_chunk: out.len(),
                        chunks: 0,
                    });
                    i = p;
                }
                let r = run.as_mut().unwrap();
                if i >= lim {
                    at = base + i as u64;
                    break;
                }
                let p = i;
                let Some((stream, tag, len)) = head(&b[p..]) else {
                    unreachable!("chain positions are checked before they are entered");
                };
                let step = next(b, p, len);
                let off = base + p as u64;
                // A chunk is taken only when something valid follows it.
                if step.is_ok() || step == Err(true) {
                    let (b0, fps) = (r.base_ms, r.fps);
                    let frame_ms = move |k: u32| b0 + k as i64 * 1000 / fps as i64;
                    match tag {
                        Tag::Video if len > 0 => {
                            let pay = &b[p + 8..p + 8 + len];
                            let sc = if pay[2] == 1 { 3 } else { 4 };
                            let key = pay.get(sc).is_some_and(|x| x & 0x1F == 7);
                            if key {
                                let e = memchr::memmem::find(&pay[sc..], &[0, 0, 1])
                                    .map(|k| sc + k)
                                    .unwrap_or(pay.len().min(sc + 256));
                                if let Some(s) = codec::parse_sps(Codec::H264, &pay[sc..e]) {
                                    size.insert(stream, (s.width, s.height));
                                }
                            }
                            let k = r.count.entry(stream).or_default();
                            let (w, h) = size.get(&stream).copied().unwrap_or((0, 0));
                            out.push(Chunk {
                                off,
                                size: len as u32,
                                channel: stream,
                                kind: if key { Kind::Key } else { Kind::Frame },
                                fmt: (h << 20) | ((w & 0xFFF) << 8) | fps.min(255),
                                t_ms: frame_ms(*k),
                                hdr: 8,
                                tail: 0,
                                origin: Origin::Avi,
                            });
                            *k += 1;
                        }
                        Tag::Video => *r.count.entry(stream).or_default() += 1,
                        Tag::Audio if len >= 2 => {
                            // Placed at the video position reached so far.
                            let k = r.count.values().copied().max().unwrap_or(0);
                            out.push(Chunk {
                                off,
                                size: (len & !1) as u32,
                                channel: stream,
                                kind: Kind::Audio,
                                fmt: 0,
                                t_ms: frame_ms(k),
                                hdr: 8,
                                tail: 0,
                                origin: Origin::Avi,
                            });
                        }
                        _ => {}
                    }
                    r.chunks += 1;
                }
                match step {
                    Ok(q) => i = q,
                    Err(_) => {
                        let endp = if step == Err(true) { p + 8 + len } else { p };
                        let r = run.take().unwrap();
                        close(r, base + endp as u64, &mut out, covered, stats);
                        i = endp.max(p + 1);
                    }
                }
            }
            if last {
                break;
            }
            base = at.max(base + 1);
        }
        if let Some(r) = run.take() {
            close(r, at, &mut out, covered, stats);
        }
    }
    // Frames found before the first key frame of their stream.
    for c in &mut out {
        if c.kind != Kind::Audio && c.fmt >> 8 == 0 {
            if let Some(&(w, h)) = size.get(&c.channel) {
                c.fmt |= (h << 20) | ((w & 0xFFF) << 8);
            }
        }
    }
    out.retain(|c| c.kind == Kind::Audio || c.fmt >> 8 != 0);
    for c in &out {
        match c.kind {
            Kind::Audio => stats.audio_chunks += 1,
            _ => {
                stats.frames += 1;
                stats.key_frames += (c.kind == Kind::Key) as usize;
                *stats.per_camera.entry(c.channel as u32 + 1).or_default() += 1;
            }
        }
    }
    Ok(out)
}

fn close(r: Run, end: u64, out: &mut Vec<Chunk>, covered: &mut Ranges, stats: &mut Stats) {
    if r.chunks < MIN_CHUNKS {
        out.truncate(r.first_chunk);
        return;
    }
    stats.runs += 1;
    stats.runs_with_file_name += (r.base_ms >= KNOWN_TIME_MS) as usize;
    covered.add(r.start, end);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = tag.to_vec();
        v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn odd_chunks_with_and_without_pad() {
        let mut b = chunk(b"00dc", &[0, 0, 0, 1, 0x41]);
        let second = b.len();
        b.extend(chunk(b"01dc", &[0, 0, 0, 1, 0x41]));
        b.push(0);
        let third = b.len();
        b.extend(chunk(b"02wb", &[1, 2]));
        assert_eq!(next(&b, 0, 5), Ok(second));
        assert_eq!(next(&b, second, 5), Ok(third));
        assert_eq!(next(&b, third, 2), Err(false));
        assert_eq!(head(&b), Some((0, Tag::Video, 5)));
        assert_eq!(head(b"00dc\x05\0\0\0hello"), None);
    }

    #[test]
    fn two_video_streams_are_recognised() {
        let mut b = vec![0u8; 7];
        for _ in 0..4 {
            b.extend(chunk(b"03st", &[0; 8]));
            b.extend(chunk(b"00dc", &[0, 0, 0, 1, 0x41, 9]));
            b.extend(chunk(b"01dc", &[0, 0, 0, 1, 0x41]));
        }
        assert!(two_cameras(&b));
        let mut one = vec![];
        for _ in 0..8 {
            one.extend(chunk(b"00dc", &[0, 0, 0, 1, 0x41, 9]));
        }
        assert!(!two_cameras(&one));
    }
}
