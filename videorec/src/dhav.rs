//! Dahua DVR/NVR frames ("DHAV").
//!
//! Frame layout, as read by FFmpeg's DHAV demuxer (libavformat/dhav.c,
//! LGPL; used here as a public description, no code copied) and checked on a
//! DH-HCVR5216AN-S2 disk supplied by the user:
//! * `DHAV`, type (0xFD key frame, 0xFC other frame, 0xF0 audio, 0xF1 skip),
//!   subtype, channel (0-based), sub-number;
//! * u32 LE frame number, u32 LE total length, u32 LE packed date
//!   (sec 6 bits, min 6, hour 5, day 5, month 4, year-2000 6);
//! * u16 LE millisecond counter, u8 extension length, u8 checksum;
//! * extensions: 0x80 width/8 and height/8, 0x82 width and height (u16),
//!   0x81 codec (2, 4, 8 = H.264; 0x0C = H.265) and frame rate, others skipped;
//! * payload (Annex B for video), then the trailer `dhav` + u32 LE total length.
//!
//! On the user's disk each 2 MiB block holds one camera's frames from its
//! start; the rest of the block still holds older frames. Those older frames
//! are the "deleted" footage, so frames are collected wherever they are and
//! re-assembled per camera by time, like `.jdr` chunks.

use crate::io::{Source, le16, le32};
use crate::jdr::{Chunk, Kind, days_from_civil};
use crate::ranges::Ranges;
use anyhow::Result;
use std::collections::{HashMap, HashSet};

const MAX_FRAME: u32 = 16 << 20;
const FIXED: usize = 24;
const TRAILER: usize = 8;

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Stats {
    pub frames: usize,
    pub key_frames: usize,
    pub audio_frames: usize,
    pub h265_frames_skipped: usize,
    pub frames_without_size: usize,
    pub bad_headers: usize,
    pub per_camera: std::collections::BTreeMap<u32, usize>,
}

/// Seconds since 1970 from the packed DHAV date, if plausible.
pub fn date_secs(d: u32) -> Option<i64> {
    let (s, mi, h, day, mo, y) = (
        d & 0x3F,
        (d >> 6) & 0x3F,
        (d >> 12) & 0x1F,
        (d >> 17) & 0x1F,
        (d >> 22) & 0x0F,
        2000 + ((d >> 26) & 0x3F),
    );
    if s > 59 || mi > 59 || h > 23 || !(1..=31).contains(&day) || !(1..=12).contains(&mo) {
        return None;
    }
    Some(((days_from_civil(y as i64, mo as i64, day as i64) * 24 + h as i64) * 60 + mi as i64) * 60 + s as i64)
}

/// Fields of one frame header.
#[derive(Debug, Clone, Copy)]
pub struct Head {
    pub kind: u8,
    pub channel: u8,
    pub number: u32,
    pub len: u32,
    pub secs: i64,
    pub ext_len: usize,
    pub width: u32,
    pub height: u32,
    pub codec: u8,
    pub fps: u32,
}

/// Parses the fixed header and extensions at the start of `b`.
pub fn parse(b: &[u8]) -> Option<Head> {
    if b.len() < FIXED || &b[..4] != b"DHAV" || !matches!(b[4], 0xF0 | 0xF1 | 0xFC | 0xFD) {
        return None;
    }
    let len = le32(b, 12);
    let ext_len = b[22] as usize;
    if len < (FIXED + TRAILER) as u32 || len > MAX_FRAME || (FIXED + ext_len + TRAILER) as u32 > len {
        return None;
    }
    let mut h = Head {
        kind: b[4],
        channel: b[6],
        number: le32(b, 8),
        len,
        secs: date_secs(le32(b, 16))?,
        ext_len,
        width: 0,
        height: 0,
        codec: 0,
        fps: 0,
    };
    let ext = b.get(FIXED..FIXED + ext_len)?;
    let mut i = 0;
    while i < ext.len() {
        let t = ext[i];
        let step = match t {
            0x80 => {
                if let Some(x) = ext.get(i + 2..i + 4) {
                    h.width = 8 * x[0] as u32;
                    h.height = 8 * x[1] as u32;
                }
                4
            }
            0x81 => {
                if let Some(x) = ext.get(i + 2..i + 4) {
                    h.codec = x[0];
                    h.fps = x[1] as u32;
                }
                4
            }
            0x82 => {
                if let Some(x) = ext.get(i + 4..i + 8) {
                    h.width = le16(x, 0) as u32;
                    h.height = le16(x, 2) as u32;
                }
                8
            }
            0x83 | 0x84 | 0x85 | 0x8B | 0x94 | 0x96 | 0xA0 | 0xB2 | 0xB4 => 4,
            0x88 | 0x8C | 0x91 | 0x92 | 0x93 | 0x95 | 0x9A | 0x9B | 0xB3 => 8,
            _ => break,
        };
        i += step;
    }
    Some(h)
}

/// A frame found on disk, before times within a second are assigned.
#[derive(Debug, Clone, Copy)]
struct Found {
    off: u64,
    head: Head,
    width: u32,
    height: u32,
    fps: u32,
}

/// Scans `[start, end)` for DHAV video frames and returns them as chunks
/// (times assigned, see `finish`). Accepted frames are added to `covered`.
/// Frames found after newer recording in the same 2 MiB neighbourhood are
/// returned in the second set (by offset): older footage left in place.
pub fn scan(
    src: &Source,
    regions: &[(u64, u64)],
    covered: &mut Ranges,
    stats: &mut Stats,
) -> Result<(Vec<Chunk>, HashSet<u64>)> {
    const BUF: usize = 8 << 20;
    const LOOK: usize = 256;
    let finder = memchr::memmem::Finder::new(b"DHAV");
    let mut buf = vec![0u8; BUF + LOOK];
    let mut found: Vec<Found> = vec![];
    // Last known picture size and rate per channel (P frames may omit them).
    let mut last: HashMap<u8, (u32, u32, u32)> = HashMap::new();
    let mut trailer = [0u8; TRAILER];
    let mut done = 0u64;
    for &(start, end) in regions {
        crate::progress::region(start, done);
        done += end - start;
        let mut pos = start;
        while pos < end {
            crate::progress::at(pos);
            if crate::progress::stopped() {
                break;
            }
            let want = (BUF + LOOK).min((end - pos) as usize);
            let n = src.read_at(pos, &mut buf[..want])?;
            if n < FIXED {
                break;
            }
            let lim = if pos + n as u64 >= end { n } else { n - LOOK };
            let mut i = 0;
            let mut next = pos + lim as u64;
            while let Some(k) = finder.find(&buf[i..n]) {
                let p = i + k;
                if p >= lim {
                    break;
                }
                let off = pos + p as u64;
                let Some(h) = parse(&buf[p..n]) else {
                    i = p + 1;
                    continue;
                };
                let fend = off + h.len as u64;
                if fend > end {
                    i = p + 1;
                    continue;
                }
                // Trailer: "dhav" + the same length.
                let t = if p + h.len as usize <= n {
                    &buf[p + h.len as usize - TRAILER..p + h.len as usize]
                } else {
                    src.read_exact_at(fend - TRAILER as u64, &mut trailer)?;
                    &trailer[..]
                };
                if &t[..4] != b"dhav" || le32(t, 4) != h.len {
                    stats.bad_headers += 1;
                    i = p + 1;
                    continue;
                }
                covered.add(off, fend);
                match h.kind {
                    0xFC | 0xFD => {
                        let e = last.entry(h.channel).or_insert((0, 0, 0));
                        if h.width > 0 && h.height > 0 {
                            (e.0, e.1) = (h.width, h.height);
                        }
                        if h.fps > 0 {
                            e.2 = h.fps;
                        }
                        let (w, hh, f) = *e;
                        if h.codec == 0x0C {
                            stats.h265_frames_skipped += 1;
                        } else if w == 0 || hh == 0 {
                            stats.frames_without_size += 1;
                        } else {
                            found.push(Found {
                                off,
                                head: h,
                                width: w,
                                height: hh,
                                fps: if f == 0 { 25 } else { f },
                            });
                        }
                    }
                    0xF0 => stats.audio_frames += 1,
                    _ => {}
                }
                if p + h.len as usize >= lim {
                    next = fend;
                    break;
                }
                i = p + h.len as usize;
            }
            let np = next.max(pos + 1).min(end);
            pos = np;
        }
    }
    Ok(finish(found, stats))
}

fn finish(mut found: Vec<Found>, stats: &mut Stats) -> (Vec<Chunk>, HashSet<u64>) {
    // Older footage: a frame more than an hour older than the newest frame
    // within the 2 MiB before it on disk.
    found.sort_by_key(|f| f.off);
    let mut older = HashSet::new();
    let mut win: std::collections::VecDeque<(u64, i64)> = Default::default();
    for f in &found {
        while win.front().is_some_and(|w| w.0 + (2 << 20) < f.off) {
            win.pop_front();
        }
        if win.iter().any(|w| w.1 > f.head.secs + 3600) {
            older.insert(f.off);
        }
        win.push_back((f.off, f.head.secs));
    }
    // Same frame stored twice: keep one.
    found.sort_by_key(|f| (f.head.channel, f.head.secs, f.head.number, f.head.len, f.off));
    found.dedup_by_key(|f| (f.head.channel, f.head.secs, f.head.number, f.head.len));
    // Times within a second: frames in recording order, spaced by the rate.
    let mut out = Vec::with_capacity(found.len());
    let mut k = 0;
    while k < found.len() {
        let (ch, s) = (found[k].head.channel, found[k].head.secs);
        let mut j = k;
        while j < found.len() && found[j].head.channel == ch && found[j].head.secs == s {
            j += 1;
        }
        for (r, f) in found[k..j].iter().enumerate() {
            // More frames in one second than the nominal rate: spread them.
            let step = (1000 / f.fps.max(1) as i64).min(1000 / (j - k) as i64);
            let ms = (r as i64 * step).min(999);
            let hdr = (FIXED + f.head.ext_len) as u16;
            stats.frames += 1;
            stats.key_frames += (f.head.kind == 0xFD) as usize;
            *stats.per_camera.entry(f.head.channel as u32 + 1).or_default() += 1;
            out.push(Chunk {
                off: f.off,
                size: f.head.len - hdr as u32 - TRAILER as u32,
                channel: f.head.channel,
                kind: if f.head.kind == 0xFD { Kind::Key } else { Kind::Frame },
                fmt: (f.height << 20) | ((f.width & 0xFFF) << 8) | f.fps.min(255),
                t_ms: s * 1000 + ms,
                hdr,
                tail: TRAILER as u16,
                dahua: true,
            });
        }
        k = j;
    }
    (out, older)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_from_disk() {
        // First frame header of a block on the user's DH-HCVR5216AN-S2 disk.
        let b: Vec<u8> = vec![
            0x44, 0x48, 0x41, 0x56, 0xFD, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x3D, 0x87,
            0x02, 0x00, 0xAB, 0xB1, 0x6A, 0x61, 0x6F, 0x2B, 0x14, 0xBB, 0x80, 0x00, 0xA0, 0x5A,
            0x81, 0x1E, 0x08, 0x1E, 0x88, 0xDA, 0xA8, 0xB0, 0x7A, 0x00, 0x00, 0x00, 0x94, 0x02,
            0x00, 0x00,
        ];
        let h = parse(&b).expect("valid");
        assert_eq!((h.kind, h.channel, h.len, h.ext_len), (0xFD, 0, 165693, 20));
        assert_eq!((h.width, h.height, h.codec, h.fps), (1280, 720, 8, 30));
        assert_eq!(crate::jdr::format_time(h.secs * 1000), "2024-05-21T11:06:43.000");
    }
}
