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

use crate::avi::AviWriter;
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
    progress: &mut crate::progress::Progress,
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
        let np = next.max(pos + 1).min(end);
        progress.add(np - pos);
        pos = np;
    }
    Ok(())
}

/// Re-reads chunk headers along runs found by an earlier `scan` (from a
/// saved session): one small read per chunk instead of reading all data.
pub fn walk(
    src: &Source,
    runs: &[(u64, u64)],
    chunks: &mut Vec<Chunk>,
    progress: &mut crate::progress::Progress,
) -> Result<usize> {
    let mut broken = 0;
    let mut b = [0u8; HEADER + 4];
    for &(start, end) in runs {
        let mut pos = start;
        while pos + (HEADER as u64) < end {
            if src.read_at(pos, &mut b)? < b.len() {
                break;
            }
            match parse_header(&b, pos) {
                Some(c) if c.end() <= end => {
                    chunks.push(c);
                    pos = c.end();
                }
                _ => {
                    broken += 1;
                    break;
                }
            }
        }
        progress.add(end - start);
    }
    Ok(broken)
}

/// Writes clips and marks those recorded before the current ring pass of
/// the `.jdr` file they start in. `files` holds (start, end) of each live
/// `.jdr` file. Returns (items, discarded segments, earlier-pass clips).
pub fn clips_with_pass_marks(
    src: &Source,
    chunks: &[Chunk],
    out: &Path,
    first_id: usize,
    o: &ClipOpts,
    stats: &mut Stats,
    files: &[(u64, u64)],
) -> Result<(Vec<Recovered>, usize, usize)> {
    let (mut got, discarded) = write_clips(src, chunks, out, first_id, o, stats)?;
    // Time of the first chunk of each live file = start of the pass now in
    // that file; clips there with earlier times are left from a previous pass.
    let first_time: std::collections::HashMap<u64, i64> =
        chunks.iter().map(|c| (c.off, c.t_ms)).collect();
    let mut files: Vec<(u64, u64, i64)> = files
        .iter()
        .filter_map(|&(s, e)| first_time.get(&(s + 0x800)).map(|&t| (s, e, t)))
        .collect();
    files.sort_unstable();
    let mut older = 0;
    for r in got.iter_mut() {
        let i = files.partition_point(|f| f.0 <= r.source_start);
        let Some(&(_, fend, t0)) = i.checked_sub(1).and_then(|k| files.get(k)) else {
            continue;
        };
        if r.source_start < fend && r.start_time.as_deref() < Some(format_time(t0).as_str()) {
            older += 1;
            r.notes.push_str(&format!(
                "; recorded before the current pass of this file began ({}) — left over from an earlier pass of the ring",
                format_time(t0)
            ));
        }
    }
    Ok((got, discarded, older))
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
    /// Frames written although no key frame precedes them (damaged picture).
    pub frames_kept_without_key: u64,
    /// Frames dropped because no key frame precedes them in their segment.
    pub frames_without_key: u64,
    /// Frames dropped because their clip was shorter than `min_frames`.
    pub frames_in_short_clips: u64,
}

pub type Stats = std::collections::BTreeMap<String, MonthStats>;

fn month(t_ms: i64) -> String {
    format_time(t_ms)[..7].to_string()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Mp4,
    Avi,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    None,
    Camera,
    Date,
    CameraDate,
}

#[derive(Clone, Debug)]
pub struct ClipOpts {
    pub max_gap_ms: i64,
    pub min_frames: usize,
    pub keep_without_key: bool,
    pub dry_run: bool,
    pub format: Format,
    pub group: Group,
    /// Recorder-time window, milliseconds since 1970 (inclusive).
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    /// Camera numbers (channel + 1) to keep; empty keeps all.
    pub cameras: Vec<u32>,
    /// Add the recorder's audio (AD chunks) to each clip.
    pub audio: bool,
}

/// Audio in AD chunks, as measured on a G-NET CATCHON card: mono signed
/// 16-bit little-endian PCM, 512 samples per 1024-byte chunk, one chunk about
/// every 64 ms, so 8000 Hz. The tag digit of AD chunks does not separate
/// microphones: chunks of all digits form one stream in time order.
pub const AUDIO_RATE: u32 = 8000;

/// PCM for `[t0, t1)` built from AD chunks sorted by time. Gaps longer than
/// 200 ms are filled with silence so sound stays in step with the video.
/// Returns (PCM bytes, chunks used).
fn audio_for(src: &Source, audio: &[Chunk], t0: i64, t1: i64) -> Result<(Vec<u8>, usize)> {
    let block_ms = 512 * 1000 / AUDIO_RATE as i64;
    let lo = audio.partition_point(|a| a.t_ms < t0 - block_ms / 2);
    let hi = audio.partition_point(|a| a.t_ms < t1);
    let mut pcm: Vec<u8> = vec![];
    let slack = AUDIO_RATE as i64 / 5 * 2;
    for a in &audio[lo..hi] {
        let at = ((a.t_ms - t0).max(0) * AUDIO_RATE as i64 / 1000) * 2;
        if at > pcm.len() as i64 + slack {
            pcm.resize(at as usize, 0);
        }
        let p = src.read_vec(a.off + HEADER as u64, a.size as usize)?;
        pcm.extend_from_slice(&p[..p.len() & !1]);
    }
    Ok((pcm, hi - lo))
}

/// Parses `YYYY-MM-DD` or `YYYY-MM-DD hh:mm[:ss]` (also with `T`). A date
/// alone means its start, or its end when `end` is set.
pub fn parse_time(s: &str, end: bool) -> Option<i64> {
    let s = s.trim().replace('T', " ");
    let (d, t) = s.split_once(' ').unwrap_or((&s, ""));
    let mut dp = d.split('-').map(|x| x.parse::<i64>());
    let (y, m, dd) = (dp.next()?.ok()?, dp.next()?.ok()?, dp.next()?.ok()?);
    if dp.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&dd) {
        return None;
    }
    let day = days_from_civil(y, m, dd) * 86_400_000;
    if t.is_empty() {
        return Some(if end { day + 86_399_999 } else { day });
    }
    let tp: Vec<i64> = t
        .split(':')
        .map(|x| x.parse().ok())
        .collect::<Option<_>>()?;
    let (h, mi, sec) = match tp[..] {
        [h, mi] => (h, mi, None),
        [h, mi, sec] => (h, mi, Some(sec)),
        _ => return None,
    };
    if h > 23 || mi > 59 || sec.is_some_and(|x| x > 59) {
        return None;
    }
    let base = day + (h * 60 + mi) * 60_000 + sec.unwrap_or(0) * 1000;
    Some(match (end, sec) {
        (true, None) => base + 59_999,
        (true, Some(_)) => base + 999,
        _ => base,
    })
}

/// Parses camera lists like `1,3,5-8`.
pub fn parse_cameras(s: &str) -> Option<Vec<u32>> {
    let mut v = vec![];
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b): (u32, u32) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
                if a > b {
                    return None;
                }
                v.extend(a..=b);
            }
            None => v.push(part.parse().ok()?),
        }
    }
    Some(v)
}

pub fn write_clips(
    src: &Source,
    chunks: &[Chunk],
    out: &Path,
    first_id: usize,
    o: &ClipOpts,
    stats: &mut Stats,
) -> Result<(Vec<Recovered>, usize)> {
    let (max_gap_ms, min_frames, keep_without_key, dry_run) =
        (o.max_gap_ms, o.min_frames, o.keep_without_key, o.dry_run);
    let mut items = vec![];
    let mut discarded = 0;
    let mut channels: Vec<u8> = chunks.iter().map(|c| c.channel).collect();
    channels.sort_unstable();
    channels.dedup();
    let mut id = first_id;
    let mut audio: Vec<Chunk> = if o.audio {
        chunks
            .iter()
            .filter(|c| c.kind == Kind::Audio && c.size >= 2 && c.size % 2 == 0)
            .copied()
            .collect()
    } else {
        vec![]
    };
    audio.sort_by_key(|c| (c.t_ms, c.off));
    audio.dedup_by_key(|c| c.t_ms);
    for ch in channels {
        let mut v: Vec<&Chunk> = chunks
            .iter()
            .filter(|c| c.channel == ch && matches!(c.kind, Kind::Key | Kind::Frame))
            .filter(|c| {
                o.from_ms.is_none_or(|f| c.t_ms >= f)
                    && o.to_ms.is_none_or(|t| c.t_ms <= t)
                    && (o.cameras.is_empty() || o.cameras.contains(&(c.channel as u32 + 1)))
            })
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
            // A clip starts at a key frame; segments with none are kept only
            // with `keep_without_key`.
            let k = seg
                .iter()
                .position(|c| c.kind == Kind::Key)
                .unwrap_or(seg.len());
            let no_key = k == seg.len();
            // Frames before the first key frame are kept (damaged picture)
            // only with `keep_without_key`.
            let damaged = k > 0 && keep_without_key;
            let seg = if keep_without_key {
                seg
            } else {
                for c in &seg[..k] {
                    stats.entry(month(c.t_ms)).or_default().frames_without_key += 1;
                }
                if no_key {
                    discarded += 1;
                    continue;
                }
                &seg[k..]
            };
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
            // Parameter sets: from the clip's own key frame, else from the
            // nearest key frame of the same channel and format.
            let params = if no_key {
                let t = seg[0].t_ms;
                let mut cands: Vec<&&Chunk> = v
                    .iter()
                    .filter(|c| c.kind == Kind::Key && c.fmt == seg[0].fmt)
                    .collect();
                cands.sort_by_key(|c| (c.t_ms - t).abs());
                let mut found = None;
                for c in cands.into_iter().take(8) {
                    if let Some(p) = params_of(src, c)? {
                        found = Some(p);
                        break;
                    }
                }
                found
            } else {
                params_of(src, seg[if damaged { k } else { 0 }])?
            };
            let Some(params) = params else {
                discarded += 1;
                continue;
            };
            for (n, c) in seg.iter().enumerate() {
                let m = stats.entry(month(c.t_ms)).or_default();
                if damaged && n < k {
                    m.frames_kept_without_key += 1;
                } else {
                    m.frames_kept += 1;
                }
            }
            if dry_run {
                continue;
            }
            match write_one(
                src,
                seg,
                out,
                id,
                params,
                if damaged { k } else { 0 },
                o,
                &audio,
            )? {
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

/// SPS and PPS carried in a key-frame chunk.
fn params_of(src: &Source, c: &Chunk) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
    let payload = src.read_vec(c.off + HEADER as u64, c.size as usize)?;
    let nals = split_nals(&payload);
    let sps = nals.iter().find(|n| n[0] & 0x1F == 7).map(|n| n.to_vec());
    let pps = nals.iter().find(|n| n[0] & 0x1F == 8).map(|n| n.to_vec());
    Ok(match (sps, pps) {
        (Some(s), Some(p)) if codec::parse_sps(Codec::H264, &s).is_some() => Some((s, p)),
        _ => None,
    })
}

#[allow(clippy::too_many_arguments)]
fn write_one(
    src: &Source,
    seg: &[&Chunk],
    out: &Path,
    id: usize,
    params: (Vec<u8>, Vec<u8>),
    // Number of leading frames with no key frame before them.
    lead: usize,
    o: &ClipOpts,
    audio: &[Chunk],
) -> Result<Option<Recovered>> {
    let no_key = lead > 0;
    let first = seg[0];
    let (sps, pps) = (vec![params.0], vec![params.1]);
    let cfg = VideoConfig {
        codec: Codec::H264,
        length_size: 4,
        vps: vec![],
        sps,
        pps,
        fps: Some(first.fps() as f64),
        origin: if no_key {
            if lead == seg.len() {
                "borrowed from another key frame of the same channel and format (jdr)".into()
            } else {
                "in-band (jdr)".into()
            }
        } else {
            "in-band (jdr)".into()
        },
    };
    let Some(entry) = mp4::sample_entry(&cfg) else {
        return Ok(None);
    };
    let start = format_time(first.t_ms);
    let cam = format!("CAM{}", first.channel as u32 + 1);
    let sub = match o.group {
        Group::None => String::new(),
        Group::Camera => format!("{cam}/"),
        Group::Date => format!("{}/", &start[..10]),
        Group::CameraDate => format!("{cam}/{}/", &start[..10]),
    };
    std::fs::create_dir_all(out.join(&sub))?;
    let stem = format!(
        "jdr_{id:05}_{cam}_{}",
        start.replace([':', '-'], "").replace(['T', '.'], "_")
    );
    let ext = match o.format {
        Format::Mp4 => "mp4",
        Format::Avi => "avi",
    };
    let rel = format!("{sub}{stem}.{ext}");
    let path = out.join(&rel);
    let fps = first.fps().max(1);
    let nominal = 1000 / fps;
    let mut durations = Vec::with_capacity(seg.len());
    let mut pieces = 1;
    let mut keys = 0;
    enum W {
        Mp4(Mp4Writer),
        Avi(AviWriter),
    }
    let last = seg[seg.len() - 1];
    let (pcm, audio_chunks) = audio_for(src, audio, first.t_ms, last.t_ms + nominal as i64)?;
    let mut w = match o.format {
        Format::Mp4 => W::Mp4(Mp4Writer::create(&path)?),
        Format::Avi if pcm.is_empty() => W::Avi(AviWriter::create(
            &path,
            *b"H264",
            first.width(),
            first.height(),
            fps,
            1,
        )?),
        Format::Avi => W::Avi(AviWriter::create_with_audio(
            &path,
            *b"H264",
            first.width(),
            first.height(),
            fps,
            1,
            AUDIO_RATE,
        )?),
    };
    // Audio is interleaved about every half second of video time.
    let mut pcm_done = 0usize;
    for (k, c) in seg.iter().enumerate() {
        let upto = ((((c.t_ms - first.t_ms) + 500) * AUDIO_RATE as i64 / 1000) * 2) as usize;
        if k == 0 || upto >= pcm_done + AUDIO_RATE as usize {
            let e = upto.min(pcm.len());
            if e > pcm_done {
                match &mut w {
                    W::Mp4(w) => w.add_audio(&pcm[pcm_done..e], AUDIO_RATE)?,
                    W::Avi(w) => w.add_audio(&pcm[pcm_done..e])?,
                }
                pcm_done = e;
            }
        }
        let p = src.read_vec(c.off + HEADER as u64, c.size as usize)?;
        let key = c.kind == Kind::Key;
        keys += key as usize;
        match &mut w {
            W::Mp4(w) => {
                let mut data = vec![];
                for n in split_nals(&p) {
                    data.extend_from_slice(&(n.len() as u32).to_be_bytes());
                    data.extend_from_slice(n);
                }
                w.add_sample(&data, key)?;
            }
            W::Avi(w) => {
                if k == 0 && lead > 0 {
                    // Parameter sets in band, so the stream can be decoded.
                    let mut data = vec![];
                    for n in cfg.sps.iter().chain(&cfg.pps) {
                        data.extend_from_slice(&[0, 0, 0, 1]);
                        data.extend_from_slice(n);
                    }
                    data.extend_from_slice(&p);
                    w.add_frame(&data, key)?;
                } else {
                    w.add_frame(&p, key)?;
                }
            }
        }
        let d = match seg.get(k + 1) {
            Some(nx) => (nx.t_ms - c.t_ms).clamp(1, 10_000) as u32,
            None => nominal,
        };
        durations.push(d);
        if k > 0 && c.off < seg[k - 1].off || (k > 0 && c.off > seg[k - 1].end() + (4 << 20)) {
            pieces += 1;
        }
    }
    if pcm_done < pcm.len() {
        match &mut w {
            W::Mp4(w) => w.add_audio(&pcm[pcm_done..], AUDIO_RATE)?,
            W::Avi(w) => w.add_audio(&pcm[pcm_done..])?,
        }
    }
    match w {
        W::Mp4(w) => w.finish_timed(&entry, 1000, &durations)?,
        W::Avi(w) => w.finish()?,
    }
    let size = std::fs::metadata(&path)?.len();
    let sound = if pcm.is_empty() {
        "no audio found for this time".to_string()
    } else {
        format!(
            "audio: {audio_chunks} AD chunks, {:.1} s of 16-bit PCM at {AUDIO_RATE} Hz",
            pcm.len() as f64 / 2.0 / AUDIO_RATE as f64
        )
    };
    Ok(Some(Recovered {
        id,
        kind: "jdr_clip".into(),
        output: rel,
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
        status: if no_key { "partial" } else { "ok" }.into(),
        notes: if no_key {
            format!(
                "timestamps from recorder; {sound}; first {lead} of {} frames have no key frame before them, so their picture is damaged",
                seg.len()
            )
        } else {
            format!("timestamps from recorder; {sound}")
        },
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
