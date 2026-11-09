//! G-NET dashcam `.jdr` recordings (e.g. CATCHON series).
//!
//! Layout as observed on a user-supplied card image (no vendor documentation;
//! derived from the recorded data itself, not from any vendor software):
//! * file header magic `1NAJ`, an index block `1VEJ` at 0x200, chunks from 0x800;
//! * each chunk has a 28-byte header followed by its payload:
//!   - 4 bytes tag: two ASCII digits (channel) + `VI` (video, key frame),
//!     `VP` (video, other frame), `AD` (audio) or `SE` (sensor);
//!   - u32 LE payload size;
//!   - u32 LE format: height << 20 | width << 8 | fps (video);
//!   - 8 × u16 LE time: year, month, (unused), day, hour, minute, second, ms;
//! * video payloads are Annex B H.264; `VI` carries SPS + PPS + IDR.
//!
//! The recorder pre-allocates fixed-size files and overwrites them in a ring,
//! so footage from earlier passes survives in the parts of each file the
//! current pass has not reached. Chunks are therefore scanned across the
//! whole volume, regardless of files, and re-assembled per channel by time.

use crate::codec::{self, Codec};
use crate::io::{Source, le16, le32};
use crate::mp4::{self, Mp4Writer, VideoConfig};
use crate::ranges::Ranges;
use crate::report::{self, Recovered};
use anyhow::Result;
use std::path::Path;

pub const HEADER: usize = 28;
const MAX_PAYLOAD: u32 = 8 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Key,
    Frame,
    Audio,
    Sensor,
}

#[derive(Debug, Clone, Copy)]
pub struct Chunk {
    /// Offset of the 28-byte header in the source.
    pub off: u64,
    pub size: u32,
    pub channel: u8,
    pub kind: Kind,
    pub fmt: u32,
    /// Milliseconds since 1970-01-01 of the recorder's local clock.
    pub t_ms: i64,
}

impl Chunk {
    pub fn width(&self) -> u32 {
        (self.fmt >> 8) & 0xFFF
    }
    pub fn height(&self) -> u32 {
        self.fmt >> 20
    }
    pub fn fps(&self) -> u32 {
        self.fmt & 0xFF
    }
    pub fn end(&self) -> u64 {
        self.off + HEADER as u64 + self.size as u64
    }
}

/// Days from 1970-01-01 for a proleptic Gregorian date (H. Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub fn format_time(t_ms: i64) -> String {
    let days = t_ms.div_euclid(86_400_000);
    let rem = t_ms.rem_euclid(86_400_000);
    // civil_from_days
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    let (h, mi, s, ms) = (
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000,
    );
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{ms:03}")
}

/// Parses a chunk header at the start of `b` (which must also hold the first
/// 4 payload bytes for video). Returns None unless every field is plausible.
pub fn parse_header(b: &[u8], off: u64) -> Option<Chunk> {
    if b.len() < HEADER + 4 || !b[0].is_ascii_digit() || !b[1].is_ascii_digit() {
        return None;
    }
    let kind = match &b[2..4] {
        b"VI" => Kind::Key,
        b"VP" => Kind::Frame,
        b"AD" => Kind::Audio,
        b"SE" => Kind::Sensor,
        _ => return None,
    };
    let size = le32(b, 4);
    let fmt = le32(b, 8);
    let (y, mo, d, h, mi, s, ms) = (
        le16(b, 12),
        le16(b, 14),
        le16(b, 18),
        le16(b, 20),
        le16(b, 22),
        le16(b, 24),
        le16(b, 26),
    );
    if !(2000..=2099).contains(&y)
        || !(1..=12).contains(&mo)
        || !(1..=31).contains(&d)
        || h > 23
        || mi > 59
        || s > 59
        || ms > 999
    {
        return None;
    }
    let c = Chunk {
        off,
        size,
        channel: (b[0] - b'0') * 10 + (b[1] - b'0'),
        kind,
        fmt,
        t_ms: ((days_from_civil(y as i64, mo as i64, d as i64) * 24 + h as i64) * 60 + mi as i64)
            * 60_000
            + s as i64 * 1000
            + ms as i64,
    };
    match kind {
        Kind::Key | Kind::Frame => {
            let (w, hh, f) = (c.width(), c.height(), c.fps());
            let annexb =
                b[HEADER..HEADER + 4] == [0, 0, 0, 1] || b[HEADER..HEADER + 3] == [0, 0, 1];
            if !(8..=MAX_PAYLOAD).contains(&size)
                || !(160..=4096).contains(&w)
                || !(120..=4096).contains(&hh)
                || !(1..=120).contains(&f)
                || !annexb
            {
                return None;
            }
        }
        Kind::Audio | Kind::Sensor => {
            if size == 0 || size > 1 << 16 {
                return None;
            }
        }
    }
    Some(c)
}

/// Scans `[start, end)` for chunks. Each accepted chunk's bytes are added to
/// `covered`.
pub fn scan(
    src: &Source,
    start: u64,
    end: u64,
    chunks: &mut Vec<Chunk>,
    covered: &mut Ranges,
) -> Result<()> {
    const CHUNK: usize = 8 << 20;
    let mut pos = start;
    let mut buf = vec![0u8; CHUNK + HEADER + 4];
    while pos < end {
        let want = (CHUNK + HEADER + 4).min((end - pos) as usize);
        let n = src.read_at(pos, &mut buf[..want])?;
        if n < HEADER + 4 {
            break;
        }
        let lim = n - (HEADER + 4) + 1;
        let mut i = 0;
        let mut next = pos + lim as u64;
        while i < lim {
            // Cheap pre-filter on the tag letters.
            let t = buf[i + 2];
            if (t == b'V' || t == b'A' || t == b'S')
                && buf[i].is_ascii_digit()
                && let Some(c) = parse_header(&buf[i..i + HEADER + 4], pos + i as u64)
                && c.end() <= end
            {
                covered.add(c.off, c.end());
                chunks.push(c);
                let skip = HEADER + c.size as usize;
                if i + skip >= lim {
                    next = c.end();
                    break;
                }
                i += skip;
                continue;
            }
            i += 1;
        }
        pos = next.max(pos + 1);
    }
    Ok(())
}

/// Splits an Annex B payload into NAL units (without start codes).
fn split_nals(p: &[u8]) -> Vec<&[u8]> {
    let f = memchr::memmem::Finder::new(&[0u8, 0, 1]);
    let starts: Vec<usize> = f.find_iter(p).map(|i| i + 3).collect();
    let mut out = vec![];
    for (k, &s) in starts.iter().enumerate() {
        let mut e = starts.get(k + 1).map(|&n| n - 3).unwrap_or(p.len());
        while e > s && p[e - 1] == 0 {
            e -= 1;
        }
        if e > s {
            out.push(&p[s..e]);
        }
    }
    out
}

/// Groups video chunks into per-channel clips and writes them as MP4.
/// Returns report items. A clip ends at a gap longer than `max_gap_ms`, at a
/// format change, or when the timeline goes backwards.
/// Per-month frame counts (by recorder time, all channels).
#[derive(Debug, Default, serde::Serialize)]
pub struct MonthStats {
    pub video_chunks: u64,
    pub key_frames: u64,
    /// Frames written to clips.
    pub frames_kept: u64,
    /// Frames dropped because no key frame precedes them in their segment.
    pub frames_without_key: u64,
    /// Frames dropped because their clip was shorter than `min_frames`.
    pub frames_in_short_clips: u64,
}

pub type Stats = std::collections::BTreeMap<String, MonthStats>;

fn month(t_ms: i64) -> String {
    format_time(t_ms)[..7].to_string()
}

#[allow(clippy::too_many_arguments)]
pub fn write_clips(
    src: &Source,
    chunks: &[Chunk],
    out: &Path,
    first_id: usize,
    max_gap_ms: i64,
    min_frames: usize,
    dry_run: bool,
    stats: &mut Stats,
) -> Result<(Vec<Recovered>, usize)> {
    let mut items = vec![];
    let mut discarded = 0;
    let mut channels: Vec<u8> = chunks.iter().map(|c| c.channel).collect();
    channels.sort_unstable();
    channels.dedup();
    let mut id = first_id;
    for ch in channels {
        let mut v: Vec<&Chunk> = chunks
            .iter()
            .filter(|c| c.channel == ch && matches!(c.kind, Kind::Key | Kind::Frame))
            .collect();
        v.sort_by_key(|c| (c.t_ms, c.off));
        v.dedup_by_key(|c| (c.t_ms, c.size));
        for c in &v {
            let m = stats.entry(month(c.t_ms)).or_default();
            m.video_chunks += 1;
            m.key_frames += (c.kind == Kind::Key) as u64;
        }
        let mut i = 0;
        while i < v.len() {
            let mut j = i + 1;
            while j < v.len() && v[j].fmt == v[i].fmt && v[j].t_ms - v[j - 1].t_ms <= max_gap_ms {
                j += 1;
            }
            let seg = &v[i..j];
            i = j;
            // A clip must start at a key frame.
            let k = seg
                .iter()
                .position(|c| c.kind == Kind::Key)
                .unwrap_or(seg.len());
            for c in &seg[..k] {
                stats.entry(month(c.t_ms)).or_default().frames_without_key += 1;
            }
            if k == seg.len() {
                discarded += 1;
                continue;
            }
            let seg = &seg[k..];
            if seg.len() < min_frames {
                for c in seg {
                    stats
                        .entry(month(c.t_ms))
                        .or_default()
                        .frames_in_short_clips += 1;
                }
                discarded += 1;
                continue;
            }
            for c in seg {
                stats.entry(month(c.t_ms)).or_default().frames_kept += 1;
            }
            if dry_run {
                continue;
            }
            match write_one(src, seg, out, id)? {
                Some(r) => {
                    items.push(r);
                    id += 1;
                }
                None => discarded += 1,
            }
        }
    }
    Ok((items, discarded))
}

fn write_one(src: &Source, seg: &[&Chunk], out: &Path, id: usize) -> Result<Option<Recovered>> {
    let first = seg[0];
    let payload = src.read_vec(first.off + HEADER as u64, first.size as usize)?;
    let nals = split_nals(&payload);
    let sps: Vec<Vec<u8>> = nals
        .iter()
        .filter(|n| n[0] & 0x1F == 7)
        .map(|n| n.to_vec())
        .take(1)
        .collect();
    let pps: Vec<Vec<u8>> = nals
        .iter()
        .filter(|n| n[0] & 0x1F == 8)
        .map(|n| n.to_vec())
        .take(1)
        .collect();
    if sps.is_empty() || pps.is_empty() || codec::parse_sps(Codec::H264, &sps[0]).is_none() {
        return Ok(None);
    }
    let cfg = VideoConfig {
        codec: Codec::H264,
        length_size: 4,
        vps: vec![],
        sps,
        pps,
        fps: Some(first.fps() as f64),
        origin: "in-band (jdr)".into(),
    };
    let Some(entry) = mp4::sample_entry(&cfg) else {
        return Ok(None);
    };
    let start = format_time(first.t_ms);
    let stem = format!(
        "jdr_{id:05}_CH{:02}_{}",
        first.channel,
        start.replace([':', '-'], "").replace(['T', '.'], "_")
    );
    let path = out.join(format!("{stem}.mp4"));
    let mut w = Mp4Writer::create(&path)?;
    let nominal = 1000 / first.fps().max(1);
    let mut durations = Vec::with_capacity(seg.len());
    let mut pieces = 1;
    let mut keys = 0;
    for (k, c) in seg.iter().enumerate() {
        let p = src.read_vec(c.off + HEADER as u64, c.size as usize)?;
        let mut data = vec![];
        for n in split_nals(&p) {
            data.extend_from_slice(&(n.len() as u32).to_be_bytes());
            data.extend_from_slice(n);
        }
        let key = c.kind == Kind::Key;
        keys += key as usize;
        w.add_sample(&data, key)?;
        let d = match seg.get(k + 1) {
            Some(nx) => (nx.t_ms - c.t_ms).clamp(1, 10_000) as u32,
            None => nominal,
        };
        durations.push(d);
        if k > 0 && c.off < seg[k - 1].off || (k > 0 && c.off > seg[k - 1].end() + (4 << 20)) {
            pieces += 1;
        }
    }
    w.finish_timed(&entry, 1000, &durations)?;
    let last = seg[seg.len() - 1];
    let size = std::fs::metadata(&path)?.len();
    Ok(Some(Recovered {
        id,
        kind: "jdr_clip".into(),
        output: path.file_name().unwrap().to_string_lossy().into(),
        sha256: report::sha256_file(&path)?,
        size,
        source_start: first.off,
        source_end: last.end(),
        codec: Some("h264".into()),
        width: Some(first.width()),
        height: Some(first.height()),
        frames: Some(seg.len()),
        keyframes: Some(keys),
        duration_s: Some(durations.iter().map(|&d| d as f64).sum::<f64>() / 1000.0),
        pieces: Some(pieces),
        channel: Some(first.channel),
        start_time: Some(start),
        end_time: Some(format_time(last.t_ms)),
        config_source: Some(cfg.origin),
        status: "ok".into(),
        notes: "timestamps from recorder; video only (audio not decoded)".into(),
        ..Default::default()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_from_card() {
        // Header bytes as found at offset 0x800 of a recording.
        let mut b = vec![
            0x30, 0x30, 0x56, 0x49, 0x00, 0x8b, 0x01, 0x00, 0x1e, 0x80, 0x87, 0x43, 0xea, 0x07,
            0x0b, 0x00, 0x00, 0x00, 0x01, 0x00, 0x11, 0x00, 0x19, 0x00, 0x17, 0x00, 0x64, 0x02,
        ];
        b.extend_from_slice(&[0, 0, 0, 1]);
        let c = parse_header(&b, 0).expect("valid header");
        assert_eq!((c.channel, c.kind, c.size), (0, Kind::Key, 0x18b00));
        assert_eq!((c.width(), c.height(), c.fps()), (1920, 1080, 30));
        assert_eq!(format_time(c.t_ms), "2026-11-01T17:25:23.612");
    }
}
