//! Multi-channel dashcam MP4 data without its index: frames of all cameras
//! are interleaved in one `mdat`, so a fragment whose `moov` is lost cannot be
//! split with the container alone.
//!
//! Layout measured on the user's 256 GB card (ext4 data partition, model
//! string "KING_5CH" in bbinfo.cfg, 4 video tracks 1920x1080 + audio + text),
//! checked against the `moov` sample tables of a live file (see README):
//!
//! - every video sample is Annex-B H.264 (4-byte start codes): SPS, PPS and an
//!   IDR slice for key frames, one non-IDR slice otherwise;
//! - every video sample ends with two filler NAL units (type 12) and the
//!   number of 0xFF bytes in the first one is the camera index:
//!   `00 00 00 01 0C [FF x channel] 00 00 00 01 0C`;
//! - between samples there may be an audio sample (codec not identified,
//!   skipped) or a 25-byte text sample
//!   `00 00 "SHO" 6 bytes, 3 bytes, 01 01 01, YY MM DD hh mm ss, 00 00`
//!   with the recorder's local time, two per second.
//!
//! No public documentation of this layout was found; everything here is from
//! that card. Frames are handed to `jdr::write_clips` as chunks.

use crate::codec::{self, Codec, PpsInfo, SpsInfo};
use crate::io::Source;
use crate::jdr::{Chunk, Kind, Origin, days_from_civil};
use crate::ranges::Ranges;
use anyhow::Result;
use std::collections::{BTreeMap, HashMap};

const TERM: &[u8] = &[0, 0, 0, 1, 0x0C];
const START: &[u8] = &[0, 0, 0, 1];
/// Largest sample looked for.
const MAXF: usize = 4 << 20;
/// Largest distance between two samples still read as "next to each other"
/// (audio and text samples lie in between).
const NEAR: usize = 2048;
const MAX_CH: usize = 7;

#[derive(Debug, Default, serde::Serialize)]
pub struct Stats {
    pub frames: usize,
    pub key_frames: usize,
    pub time_records: usize,
    /// Places where the data stops being continuous (fragment borders).
    pub breaks: usize,
    pub frames_without_time: usize,
    /// Frames after a break and before the next key frame (not decodable).
    pub frames_before_key: usize,
    pub per_camera: BTreeMap<u32, usize>,
    /// Typical frames per second by the recorder's clock.
    pub fps_per_camera: BTreeMap<u32, f64>,
}

/// End of a sample: returns (length of the two filler NALs, channel).
fn terminator(b: &[u8]) -> Option<(usize, u8)> {
    if !b.starts_with(TERM) {
        return None;
    }
    let mut n = 0;
    while n <= MAX_CH && b.get(5 + n) == Some(&0xFF) {
        n += 1;
    }
    (n <= MAX_CH && b.get(5 + n..10 + n) == Some(TERM)).then_some((10 + n, n as u8))
}

/// Number of sample terminators in `buf` (used to recognise the format).
pub fn count_terminators(buf: &[u8]) -> usize {
    let f = memchr::memmem::Finder::new(TERM);
    let (mut i, mut n) = (0, 0);
    while let Some(k) = f.find(&buf[i..]) {
        match terminator(&buf[i + k..]) {
            Some((len, _)) => {
                n += 1;
                i += k + len;
            }
            None => i += k + 1,
        }
    }
    n
}

/// True when data at one of `offsets` (starts of live video files) or at
/// evenly spread places of `regions` has this layout.
pub fn detect(src: &Source, offsets: &[u64], regions: &[(u64, u64)]) -> Result<bool> {
    let mut buf = vec![0u8; 1 << 20];
    let mut check = |off: u64, lim: u64| -> Result<bool> {
        let want = buf.len().min(lim.saturating_sub(off) as usize);
        let n = src.read_at(off, &mut buf[..want])?;
        Ok(count_terminators(&buf[..n]) >= 4)
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

/// Seconds since 1970 (recorder's clock) from a text sample found in `gap`.
fn time_records(gap: &[u8], out: &mut Vec<i64>) {
    let f = memchr::memmem::Finder::new(b"SHO");
    let mut i = 0;
    while let Some(k) = f.find(&gap[i..]) {
        let p = i + k;
        i = p + 3;
        let Some(t) = gap.get(p + 12..p + 21) else {
            break;
        };
        if t[..3] != [1, 1, 1] {
            continue;
        }
        let (y, mo, d, h, mi, s) = (t[3], t[4], t[5], t[6], t[7], t[8]);
        if (1..=12).contains(&mo) && (1..=31).contains(&d) && h < 24 && mi < 60 && s < 60 && y < 100
        {
            out.push(
                days_from_civil(2000 + y as i64, mo as i64, d as i64) * 86400
                    + h as i64 * 3600
                    + mi as i64 * 60
                    + s as i64,
            );
        }
    }
}

struct Head {
    /// Start of the sample relative to the searched window.
    start: usize,
    key: bool,
    sps: Option<usize>,
    pps: Option<usize>,
    slice: usize,
}

/// The last complete sample start in `w` (data before a sample terminator):
/// SPS, PPS, IDR slice, or one slice. H.264 payload cannot contain a start
/// code, so anything before that start is an earlier, cut sample.
fn sample_head(w: &[u8]) -> Option<Head> {
    let f = memchr::memmem::Finder::new(START);
    let mut cur: Option<Head> = None;
    let mut open: Option<(usize, Option<usize>)> = None; // SPS, PPS seen, no slice yet
    let mut i = 0;
    while let Some(k) = f.find(&w[i..]) {
        let p = i + k;
        i = p + 4;
        let Some(&h) = w.get(p + 4) else { break };
        if h & 0x80 != 0 {
            continue;
        }
        match h & 0x1F {
            7 => open = Some((p, None)),
            8 => {
                if let Some(o) = open.as_mut() {
                    o.1 = Some(p);
                }
            }
            5 => {
                cur = Some(match open.take() {
                    Some((s, Some(pp))) => Head {
                        start: s,
                        key: true,
                        sps: Some(s),
                        pps: Some(pp),
                        slice: p,
                    },
                    // IDR without parameter sets: decodable only with those
                    // of another key frame; treated as a plain frame.
                    _ => Head {
                        start: p,
                        key: false,
                        sps: None,
                        pps: None,
                        slice: p,
                    },
                });
            }
            1 => {
                open = None;
                cur = Some(Head {
                    start: p,
                    key: false,
                    sps: None,
                    pps: None,
                    slice: p,
                });
            }
            _ => {}
        }
    }
    cur
}

fn nal_at(w: &[u8], p: usize) -> &[u8] {
    let s = p + 4;
    let e = memchr::memmem::find(&w[s..], START)
        .map(|k| s + k)
        .unwrap_or(w.len());
    &w[s..e]
}

#[derive(Clone, Copy)]
struct Raw {
    off: u64,
    size: u32,
    tail: u16,
    ch: u8,
    key: bool,
    /// Seconds since 1970; 0 when no time record belongs to the frame.
    sec: i64,
    /// Frames of one channel with the same number were stored one after
    /// another without interruption.
    run: u32,
    wh: (u16, u16),
}

#[derive(Default)]
struct ChState {
    sps: Option<SpsInfo>,
    pps: Option<PpsInfo>,
    wh: (u16, u16),
    frame_num: Option<u32>,
    run: u32,
    generation: u32,
}

/// Time labelling state while walking the data in disk order.
#[derive(Default)]
struct Clock {
    cur: Option<i64>,
    /// Index of the first frame not yet labelled.
    pending: usize,
    /// First record after a break and the number of frames before it; whether
    /// those belong to its second or the one before is known at the next
    /// record (two records are written per second).
    first: Option<(i64, usize)>,
}

impl Clock {
    fn settle(&mut self, frames: &mut [Raw], next: Option<i64>) {
        if let Some((sec, upto)) = self.first.take() {
            let s = if next == Some(sec) { sec - 1 } else { sec };
            for f in &mut frames[self.pending.min(upto)..upto] {
                f.sec = s;
            }
        }
    }
    fn reset(&mut self, frames: &mut [Raw]) {
        self.settle(frames, None);
        self.cur = None;
        self.pending = frames.len();
    }
    /// Returns true when the record does not continue the previous one.
    fn record(&mut self, frames: &mut [Raw], sec: i64) -> bool {
        match self.cur {
            None => {
                self.first = Some((sec, frames.len()));
                self.cur = Some(sec);
                false
            }
            Some(c) => {
                self.settle(frames, Some(sec));
                self.cur = Some(sec);
                self.pending = frames.len();
                !(c..=c + 1).contains(&sec)
            }
        }
    }
}

/// Finds the frames in `regions` and returns them as chunks with channel and
/// time. Accepted data is added to `covered`. Returns nothing (and covers
/// nothing) when no time record is found at all.
pub fn scan(
    src: &Source,
    regions: &[(u64, u64)],
    covered: &mut Ranges,
    keep_without_key: bool,
    stats: &mut Stats,
) -> Result<Vec<Chunk>> {
    const BUF: usize = 16 << 20;
    let finder = memchr::memmem::Finder::new(TERM);
    let mut buf = vec![0u8; BUF];
    let mut frames: Vec<Raw> = vec![];
    let mut chans: Vec<ChState> = (0..=MAX_CH).map(|_| ChState::default()).collect();
    let mut generation = 1u32;
    let mut cover = Ranges::new();
    let mut recs = vec![];
    let mut done = 0u64;
    for &(start, end) in regions {
        crate::progress::region(start, done);
        done += end - start;
        let mut clock = Clock {
            pending: frames.len(),
            ..Default::default()
        };
        generation += 1;
        let mut prev_end = start;
        let mut scanned = start;
        let mut base = start;
        while base < end {
            crate::progress::at(scanned);
            if crate::progress::stopped() {
                break;
            }
            let want = BUF.min((end - base) as usize);
            let n = src.read_at(base, &mut buf[..want])?;
            if n < 32 {
                break;
            }
            let last = base + n as u64 >= end;
            let lim = if last { n } else { n - 32 };
            let mut i = (scanned.max(prev_end) - base) as usize;
            while i < lim {
                let Some(k) = finder.find(&buf[i..n]) else {
                    break;
                };
                let k = i + k;
                if k >= lim {
                    break;
                }
                let Some((tlen, ch)) = terminator(&buf[k..n]) else {
                    i = k + 1;
                    continue;
                };
                let e = k + tlen;
                let p0 = (prev_end.max(base) - base) as usize;
                let far = k - p0 > MAXF;
                let ws = if far { k - MAXF } else { p0 };
                let head = sample_head(&buf[ws..k]).filter(|h| {
                    // A slice cannot contain 00 00 00/01/02 (emulation
                    // prevention): rejects start codes that are other data,
                    // such as index tables.
                    let body = &buf[ws + h.slice + 4..k];
                    let body = &body[..body.len().saturating_sub(16).min(8192)];
                    body.len() >= 4
                        && !body
                            .windows(3)
                            .any(|x| x[0] == 0 && x[1] == 0 && x[2] <= 2)
                });
                let Some(h) = head else {
                    // A cut sample: what follows belongs to another fragment.
                    stats.breaks += 1;
                    generation += 1;
                    clock.reset(&mut frames);
                    prev_end = base + e as u64;
                    i = e;
                    continue;
                };
                let s = ws + h.start;
                let gap = &buf[ws..s];
                let jump = far || gap.len() > NEAR;
                if jump {
                    stats.breaks += 1;
                    generation += 1;
                    clock.reset(&mut frames);
                }
                recs.clear();
                time_records(&gap[gap.len().saturating_sub(NEAR)..], &mut recs);
                for &sec in &recs {
                    stats.time_records += 1;
                    if clock.record(&mut frames, sec) {
                        stats.breaks += 1;
                        generation += 1;
                    }
                }
                let c = &mut chans[ch as usize];
                let w = &buf[ws..k];
                if let (Some(sp), Some(pp)) = (h.sps, h.pps) {
                    if let Some(info) = codec::parse_sps(Codec::H264, nal_at(w, sp)) {
                        c.wh = (info.width as u16, info.height as u16);
                        c.sps = Some(info);
                    }
                    c.pps = codec::parse_pps(Codec::H264, nal_at(w, pp));
                }
                let slice = &w[h.slice + 4..];
                let num = codec::slice_head(Codec::H264, slice, c.sps.as_ref(), c.pps.as_ref())
                    .and_then(|x| x.frame_num);
                let follows = match (c.frame_num, num, &c.sps) {
                    _ if h.key => true,
                    (Some(a), Some(b), Some(sp)) => {
                        b == (a + 1) & ((1u32 << sp.log2_max_frame_num.min(16)) - 1)
                    }
                    _ => false,
                };
                if c.generation != generation || !follows {
                    c.run += 1;
                    c.generation = generation;
                }
                c.frame_num = num;
                frames.push(Raw {
                    off: base + s as u64,
                    size: (k - s) as u32,
                    tail: tlen as u16,
                    ch,
                    key: h.key,
                    sec: clock.cur.unwrap_or(0),
                    run: c.run,
                    wh: c.wh,
                });
                // Audio and text samples between two samples are covered too,
                // so that later stages do not see thousands of tiny gaps.
                let from = if jump { s } else { p0 };
                cover.add(base + from as u64, base + e as u64);
                prev_end = base + e as u64;
                i = e;
            }
            scanned = base + lim as u64;
            if last {
                break;
            }
            let nb = prev_end.max((base + n as u64).saturating_sub(MAXF as u64 + 32));
            base = if nb > base { nb } else { base + lim as u64 };
        }
        clock.reset(&mut frames);
    }
    if stats.time_records == 0 {
        return Ok(vec![]);
    }
    cover.normalize();
    for (a, b) in cover.iter() {
        covered.add(a, b);
    }
    Ok(finish(frames, keep_without_key, stats))
}

fn finish(mut frames: Vec<Raw>, keep_without_key: bool, stats: &mut Stats) -> Vec<Chunk> {
    // Picture size of frames seen before the channel's first key frame.
    let mut wh: HashMap<u8, (u16, u16)> = HashMap::new();
    for f in &frames {
        if f.wh.0 > 0 {
            wh.entry(f.ch).or_insert(f.wh);
        }
    }
    for f in &mut frames {
        if f.wh.0 == 0 {
            f.wh = wh.get(&f.ch).copied().unwrap_or((0, 0));
        }
    }
    let before = frames.len();
    frames.retain(|f| f.sec != 0 && f.wh.0 > 0);
    stats.frames_without_time = before - frames.len();
    // Disk order within a run is recording order.
    frames.sort_by_key(|f| (f.ch, f.run, f.off));
    if !keep_without_key {
        let mut seen_key = false;
        let mut prev = (u8::MAX, 0u32);
        let before = frames.len();
        frames.retain(|f| {
            if (f.ch, f.run) != prev {
                prev = (f.ch, f.run);
                seen_key = false;
            }
            seen_key |= f.key;
            seen_key
        });
        stats.frames_before_key = before - frames.len();
    }
    // Runs: [a, b) of one channel stored without interruption.
    let mut runs: Vec<(usize, usize)> = vec![];
    let mut a = 0;
    for i in 1..=frames.len() {
        if i == frames.len() || (frames[i].ch, frames[i].run) != (frames[a].ch, frames[a].run) {
            runs.push((a, i));
            a = i;
        }
    }
    // The text samples give whole seconds only. The first frame after the
    // second changes is taken as the start of that second ("mark"); the frame
    // rate of a channel is frames between first and last mark over the
    // seconds between them, summed over all runs. (On the card measured, the
    // rate by the recorder's clock is about 27.7 and 13.8 frames per second,
    // while the MP4 index claims 30 and 15.)
    let marks_of = |a: usize, b: usize| -> Vec<(usize, i64)> {
        (a + 1..b)
            .filter(|&i| frames[i].sec == frames[i - 1].sec + 1)
            .map(|i| (i, frames[i].sec))
            .collect()
    };
    // Milliseconds per frame of a run with at least three seconds between
    // its first and last mark. The rate can differ between recordings of one
    // camera (driving and parking mode), so it is taken per run; shorter runs
    // use the channel's median, weighted by seconds.
    let dt_between_marks = |a: usize, b: usize, min_s: u64| -> Option<(f64, u64)> {
        let m = marks_of(a, b);
        let (f, l) = (m.first()?, m.last()?);
        let (n, s) = ((l.0 - f.0) as u64, (l.1 - f.1) as u64);
        (s >= min_s && n >= s && n <= 240 * s).then(|| (1000.0 * s as f64 / n as f64, s))
    };
    let own_dt = |a: usize, b: usize| dt_between_marks(a, b, 3);
    let mut by_ch: HashMap<u8, Vec<(f64, u64)>> = HashMap::new();
    for &(a, b) in &runs {
        if let Some(x) = dt_between_marks(a, b, 1) {
            by_ch.entry(frames[a].ch).or_default().push(x);
        }
    }
    let ch_dt: HashMap<u8, f64> = by_ch
        .into_iter()
        .map(|(ch, mut v)| {
            v.sort_by(|x, y| x.0.total_cmp(&y.0));
            let half = v.iter().map(|x| x.1).sum::<u64>() / 2;
            let mut acc = 0;
            let mut dt = v[0].0;
            for &(d, w) in &v {
                dt = d;
                acc += w;
                if acc > half {
                    break;
                }
            }
            (ch, dt)
        })
        .collect();
    let run_dt: Vec<f64> = runs
        .iter()
        .map(|&(a, b)| {
            own_dt(a, b)
                .map(|x| x.0)
                .or(ch_dt.get(&frames[a].ch).copied())
                .unwrap_or(1000.0 / 30.0)
        })
        .collect();
    // Time of every frame: a straight line through the marks of its run. If
    // the line leaves the recorded seconds by more than two seconds (frames
    // were dropped while recording), each second is laid out on its own.
    let mut times: Vec<i64> = vec![0; frames.len()];
    for (r, &(a, b)) in runs.iter().enumerate() {
        let dt = run_dt[r];
        let m = marks_of(a, b);
        let mut c: Vec<f64> = m
            .iter()
            .map(|&(i, s)| s as f64 * 1000.0 - (i - a) as f64 * dt)
            .collect();
        c.sort_by(|x, y| x.total_cmp(y));
        let c = c
            .get(c.len() / 2)
            .copied()
            .unwrap_or(frames[a].sec as f64 * 1000.0);
        let mut ok = true;
        for i in a..b {
            let t = c + (i - a) as f64 * dt;
            times[i] = t.round() as i64;
            // After the last mark there may be no further text samples.
            let after_last = m.last().is_none_or(|l| i >= l.0);
            if !after_last && (t - frames[i].sec as f64 * 1000.0 - 500.0).abs() > 2500.0 {
                ok = false;
                break;
            }
        }
        if !ok {
            let mut i = a;
            while i < b {
                let mut j = i + 1;
                while j < b && frames[j].sec == frames[i].sec {
                    j += 1;
                }
                let step = dt.min(1000.0 / (j - i) as f64);
                for k in i..j {
                    times[k] = frames[i].sec * 1000 + ((k - i) as f64 * step) as i64;
                }
                i = j;
            }
        }
        for i in a + 1..b {
            times[i] = times[i].max(times[i - 1] + 1);
        }
    }
    // Two runs of a channel that overlap by less than three seconds (the
    // uncertainty of whole-second stamps): the later one is moved behind the
    // earlier one so that their frames are not mixed when sorted by time.
    let mut order: Vec<usize> = (0..runs.len()).collect();
    order.sort_by_key(|&r| (frames[runs[r].0].ch, times[runs[r].0]));
    for w in 1..order.len() {
        let (pa, pb) = runs[order[w - 1]];
        let (a, b) = runs[order[w]];
        if frames[pa].ch != frames[a].ch {
            continue;
        }
        let dt = run_dt[order[w - 1]].round() as i64;
        let over = times[pb - 1] + dt - times[a];
        if over > 0 && over <= 3000 {
            for t in &mut times[a..b] {
                *t += over;
            }
        }
    }
    let mut out = Vec::with_capacity(frames.len());
    for (r, &(a, b)) in runs.iter().enumerate() {
        let fps = (1000.0 / run_dt[r]).round().clamp(1.0, 255.0) as u32;
        for i in a..b {
            let f = &frames[i];
            stats.frames += 1;
            stats.key_frames += f.key as usize;
            *stats.per_camera.entry(f.ch as u32 + 1).or_default() += 1;
            out.push(Chunk {
                off: f.off,
                size: f.size,
                channel: f.ch,
                kind: if f.key { Kind::Key } else { Kind::Frame },
                fmt: ((f.wh.1 as u32) << 20) | ((f.wh.0 as u32 & 0xFFF) << 8) | fps,
                t_ms: times[i],
                hdr: 0,
                tail: f.tail,
                origin: Origin::Mixed,
            });
        }
    }
    for (ch, dt) in ch_dt {
        stats.fps_per_camera.insert(ch as u32 + 1, 1000.0 / dt);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminator_gives_channel() {
        assert_eq!(terminator(&[0, 0, 0, 1, 12, 0, 0, 0, 1, 12]), Some((10, 0)));
        assert_eq!(
            terminator(&[0, 0, 0, 1, 12, 0xFF, 0xFF, 0, 0, 0, 1, 12, 9]),
            Some((12, 2))
        );
        assert_eq!(terminator(&[0, 0, 0, 1, 12, 0x80, 0, 0, 0, 1, 12]), None);
    }

    #[test]
    fn time_record_from_card() {
        // Text sample of N_20260904_142948.mp4 on the user's card.
        let mut g = vec![0u8, 0];
        g.extend_from_slice(b"SHO00015M");
        g.extend_from_slice(&[0x80, 0x80, 0x80, 1, 1, 1, 0x1A, 9, 4, 0x0E, 0x1D, 0x35, 0, 0]);
        let mut v = vec![];
        time_records(&g, &mut v);
        assert_eq!(v.len(), 1);
        assert_eq!(
            crate::jdr::format_time(v[0] * 1000),
            "2026-09-04T14:29:53.000"
        );
    }

    #[test]
    fn head_is_last_complete_sample() {
        // A cut slice, then a key frame: the key frame is the sample.
        let mut w = vec![0, 0, 0, 1, 0x41, 9, 9, 9];
        let key_at = w.len();
        w.extend_from_slice(&[0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4]);
        let h = sample_head(&w).unwrap();
        assert!(h.key);
        assert_eq!(h.start, key_at);
        assert_eq!(nal_at(&w, h.pps.unwrap()), &[0x68, 3]);
    }
}
