//! Carving of MPEG-1/MPEG-2 video (ISO/IEC 11172-2, ISO/IEC 13818-2) and of
//! MPEG program streams (ISO/IEC 13818-1, 2.5; 11172-1) from raw bytes.
//!
//! Elementary video: start codes `00 00 01 xx` cannot occur inside coded
//! data. A picture starts at `00` (with the sequence/GOP headers before it),
//! is followed by slices `01`-`AF`, and runs to the next picture, GOP or
//! sequence header. A run needs a sequence header (`B3`: picture size, frame
//! rate) and starts at an I picture; it is written as it is on the medium
//! (`.mpg`, elementary stream), with the sequence header put in front when
//! the run does not begin with one.
//!
//! Program stream: packs (`BA`) and PES packets follow each other without
//! gaps, each giving its own length, so a chain of them is copied whole.

use crate::io::Source;
use crate::ranges::Ranges;
use crate::report::{self, Recovered};
use anyhow::Result;
use std::io::Write;
use std::path::Path;

const MAX_FRAME: u64 = 4 << 20;
/// Shorter pack chains are ignored.
const MIN_PACKS: usize = 64;
/// Frame rates by frame_rate_code (13818-2 table 6-4).
const RATES: [f64; 9] = [
    0.0,
    24000.0 / 1001.0,
    24.0,
    25.0,
    30000.0 / 1001.0,
    30.0,
    50.0,
    60000.0 / 1001.0,
    60.0,
];

struct Seq {
    width: u32,
    height: u32,
    rate: u8,
    mpeg2: bool,
    header: Vec<u8>,
}

struct Frame {
    start: u64,
    end: u64,
    key: bool,
    seq: usize,
}

struct Open {
    start: u64,
    key: bool,
    seq: usize,
    /// Start of the last slice seen, and the longest slice so far.
    last_slice: Option<u64>,
    max_slice: u64,
}

struct Ps {
    start: u64,
    /// Start of the last packet (not yet confirmed by a start code after it).
    prev: u64,
    pos: u64,
    packs: usize,
    video: bool,
}

/// Scanner state, kept between consecutive windows of one region.
#[derive(Default)]
pub struct Scanner {
    seqs: Vec<Seq>,
    frames: Vec<Frame>,
    cur: Option<usize>,
    open: Option<Open>,
    pending: Option<u64>,
    /// A sequence header whose bytes are still to be stored: (index, start).
    capture: Option<(usize, u64)>,
    last_code: u64,
    ps: Option<Ps>,
    ps_until: u64,
    ps_runs: Vec<(u64, u64, usize)>,
    at: u64,
    region_end: u64,
}

/// Parses a sequence header body (after `00 00 01 B3`).
fn parse_seq(b: &[u8]) -> Option<(u32, u32, u8)> {
    if b.len() < 7 {
        return None;
    }
    let w = (b[0] as u32) << 4 | (b[1] as u32) >> 4;
    let h = ((b[1] & 0x0F) as u32) << 8 | b[2] as u32;
    let (aspect, rate) = (b[3] >> 4, b[3] & 0x0F);
    let marker = b[6] & 0x20 != 0;
    ((16..=4096).contains(&w)
        && (16..=4096).contains(&h)
        && (1..=4).contains(&aspect)
        && (1..=8).contains(&rate)
        && marker)
        .then_some((w, h, rate))
}

impl Scanner {
    /// Closes the open picture at `end`, the next start code. `video`: that
    /// code belongs to the video stream too.
    ///
    /// The last picture of a stream has no video start code after it, so it
    /// would run on into whatever follows. Where the next code is far beyond
    /// the picture's last slice (judged by its longest slice), the picture is
    /// cut there instead. A non-video code close behind the last slice means
    /// the video is packetised inside a container: not kept here.
    fn finish_open(&mut self, end: u64, video: bool) {
        let Some(o) = self.open.take() else { return };
        let Some(last) = o.last_slice else { return };
        let limit = last + (4 * o.max_slice).max(2048);
        if !video && end - last <= 65536 + 16 {
            return;
        }
        let end = end.min(limit);
        if end - o.start <= MAX_FRAME {
            self.frames.push(Frame {
                start: o.start,
                end,
                key: o.key,
                seq: o.seq,
            });
        }
    }

    fn end_ps(&mut self, end: u64) {
        if let Some(p) = self.ps.take() {
            if p.packs >= MIN_PACKS && p.video && end > p.start {
                self.ps_runs.push((p.start, end, p.packs));
            }
            self.ps_until = self.ps_until.max(end);
        }
    }

    /// Ends the region being scanned.
    fn close(&mut self) {
        if let Some(p) = &self.ps {
            // A packet that ends exactly at the region's end is complete.
            let end = if p.pos == self.region_end {
                p.pos
            } else {
                p.prev
            };
            self.end_ps(end);
        }
        self.finish_open(self.region_end, false);
        self.pending = None;
        self.capture = None;
        self.cur = None;
        self.last_code = 0;
    }

    /// Follows the pack chain while it starts before `stop`.
    fn follow_ps(&mut self, src: &Source, stop: u64, end: u64) -> Result<()> {
        let mut b = [0u8; 16];
        while let Some(p) = self.ps.as_mut() {
            if p.pos >= stop {
                return Ok(());
            }
            let n = if p.pos + 4 <= end {
                src.read_at(p.pos, &mut b)?
            } else {
                0
            };
            if n < 4 || b[..3] != [0, 0, 1] {
                let e = p.prev;
                self.end_ps(e);
                break;
            }
            let len = match b[3] {
                0xBA if n >= 14 && b[4] & 0xC0 == 0x40 => 14 + (b[13] & 7) as u64,
                0xBA if n >= 12 && b[4] & 0xF0 == 0x20 => 12,
                0xB9 => {
                    let e = p.pos + 4;
                    self.end_ps(e);
                    break;
                }
                0xBB..=0xFF if n >= 6 && b[3] != 0xBA => {
                    6 + u16::from_be_bytes([b[4], b[5]]) as u64
                }
                _ => 0,
            };
            if len == 0 || p.pos + len > end {
                let e = p.pos; // the packet before this one is confirmed
                self.end_ps(e);
                break;
            }
            p.packs += (b[3] == 0xBA) as usize;
            p.video |= (0xE0..=0xEF).contains(&b[3]);
            p.prev = p.pos;
            p.pos += len;
        }
        Ok(())
    }

    fn on_code(
        &mut self,
        src: &Source,
        at: u64,
        code: u8,
        body: &[u8],
        stop: u64,
        end: u64,
    ) -> Result<()> {
        let video = code <= 0xAF || matches!(code, 0xB2 | 0xB3 | 0xB5 | 0xB7 | 0xB8);
        if !video {
            // Not elementary video (a container's packet, other data).
            self.finish_open(at, false);
            self.pending = None;
            self.capture = None;
            if code == 0xBA {
                self.ps = Some(Ps {
                    start: at,
                    prev: at,
                    pos: at,
                    packs: 0,
                    video: false,
                });
                self.follow_ps(src, stop, end)?;
                if let Some(p) = &self.ps {
                    self.ps_until = p.pos;
                }
            }
            return Ok(());
        }
        if at - self.last_code > MAX_FRAME {
            self.finish_open(at, false);
            self.pending = None;
            self.cur = None;
            self.capture = None;
        }
        self.last_code = at;
        if let Some((i, s)) = self.capture
            && matches!(code, 0x00 | 0xB8)
        {
            if self.seqs[i].header.is_empty() && at - s <= 4096 {
                self.seqs[i].header = src.read_vec(s, (at - s) as usize)?;
            }
            self.capture = None;
        }
        match code {
            0x01..=0xAF => {
                if let Some(o) = self.open.as_mut() {
                    if let Some(l) = o.last_slice {
                        o.max_slice = o.max_slice.max(at - l);
                    }
                    o.last_slice = Some(at);
                }
            }
            0xB2 | 0xB5 => {
                if let (0xB5, Some((i, _))) = (code, self.capture) {
                    self.seqs[i].mpeg2 = true;
                }
                if self.open.is_none() {
                    self.pending.get_or_insert(at);
                }
            }
            0xB3 => {
                self.finish_open(at, true);
                match parse_seq(body) {
                    Some((w, h, rate)) => {
                        let i = match self
                            .seqs
                            .iter()
                            .position(|s| (s.width, s.height, s.rate) == (w, h, rate))
                        {
                            Some(i) => i,
                            None => {
                                self.seqs.push(Seq {
                                    width: w,
                                    height: h,
                                    rate,
                                    mpeg2: false,
                                    header: vec![],
                                });
                                self.seqs.len() - 1
                            }
                        };
                        self.cur = Some(i);
                        self.capture = Some((i, at));
                        self.pending.get_or_insert(at);
                    }
                    None => self.pending = None,
                }
            }
            0xB8 => {
                self.finish_open(at, true);
                self.pending.get_or_insert(at);
            }
            0x00 => {
                self.finish_open(at, true);
                let kind = body.get(1).map_or(0, |b| (b >> 3) & 7);
                match self.cur {
                    Some(seq) if (1..=3).contains(&kind) => {
                        self.open = Some(Open {
                            start: self.pending.take().unwrap_or(at),
                            key: kind == 1,
                            seq,
                            last_slice: None,
                            max_slice: 0,
                        })
                    }
                    _ => self.pending = None,
                }
            }
            _ => self.finish_open(at + 4, true), // B7: sequence end
        }
        Ok(())
    }

    /// Scans `[start, stop)` of a region ending at `end`. A call that starts
    /// where the previous one stopped continues the same region.
    pub fn scan_window(&mut self, src: &Source, start: u64, stop: u64, end: u64) -> Result<()> {
        const BLOCK: usize = 16 << 20;
        if start != self.at {
            self.close();
        }
        self.region_end = end;
        self.follow_ps(src, stop, end)?;
        if let Some(p) = &self.ps {
            self.ps_until = p.pos;
        }
        let finder = memchr::memmem::Finder::new(&[0, 0, 1]);
        let mut pos = start;
        while pos < stop {
            crate::progress::at(pos);
            if crate::progress::stopped() {
                break;
            }
            if self.ps_until >= stop {
                break;
            }
            pos = pos.max(self.ps_until.min(stop));
            let buf = src.read_vec(pos, (BLOCK + 64).min((end - pos) as usize))?;
            if buf.len() < 8 {
                break;
            }
            let last = buf.len() < BLOCK + 64;
            let lim = if last { buf.len() - 3 } else { BLOCK };
            let lim = lim.min((stop - pos) as usize);
            for k in finder.find_iter(&buf[..(lim + 2).min(buf.len())]) {
                if k + 4 > buf.len() {
                    break;
                }
                let at = pos + k as u64;
                if at < self.ps_until {
                    continue;
                }
                let body = &buf[k + 4..(k + 16).min(buf.len())];
                self.on_code(src, at, buf[k + 3], body, stop, end)?;
            }
            pos += lim as u64;
        }
        self.at = stop;
        if stop >= end {
            self.close();
            self.at = u64::MAX;
        }
        Ok(())
    }

    /// The region being scanned turned out to end at `pos`, where the
    /// previous window stopped.
    pub fn end_region(&mut self, pos: u64) {
        if self.at == pos {
            self.region_end = pos;
            self.close();
            self.at = u64::MAX;
        }
    }

    /// Drops what starts inside `claimed` (found to be something else).
    pub fn exclude(&mut self, claimed: &Ranges) {
        self.close();
        self.frames
            .retain(|f| claimed.containing(f.start).is_none());
        self.ps_runs.retain(|r| claimed.containing(r.0).is_none());
    }

    /// Writes the runs found (after the last window).
    #[allow(clippy::too_many_arguments)]
    pub fn finish(
        mut self,
        src: &Source,
        out: &Path,
        first_id: usize,
        max_gap: u64,
        min_frames: usize,
        fps: Option<f64>,
    ) -> Result<(Vec<Recovered>, usize)> {
        self.close();
        let mut items = vec![];
        let mut small = 0;
        for &(s, e, packs) in &self.ps_runs {
            let id = first_id + items.len();
            let name = format!("mpegps_{id:05}_{s:012x}.mpg");
            let path = out.join(&name);
            crate::copy_range(src, s, e, &path)?;
            // Picture size from the first sequence header inside, if near.
            let head = src.read_vec(s, ((e - s) as usize).min(1 << 20))?;
            let dims = memchr::memmem::find_iter(&head, &[0, 0, 1, 0xB3])
                .find_map(|k| parse_seq(&head[k + 4..]));
            items.push(Recovered {
                id,
                kind: "carved_mpeg".into(),
                output: name,
                sha256: report::sha256_file(&path)?,
                size: e - s,
                source_start: s,
                source_end: e,
                codec: Some("mpeg-ps".into()),
                width: dims.map(|d| d.0),
                height: dims.map(|d| d.1),
                pieces: Some(1),
                status: "ok".into(),
                notes: format!(
                    "MPEG program stream: unbroken chain of {packs} packs copied as found (video and any audio inside it)"
                ),
                ..Default::default()
            });
        }
        let frames = &self.frames;
        let mut i = 0;
        while i < frames.len() {
            if !frames[i].key {
                i += 1;
                continue;
            }
            let mut j = i + 1;
            while j < frames.len()
                && frames[j].seq == frames[i].seq
                && frames[j].start.saturating_sub(frames[j - 1].end) <= max_gap
            {
                j += 1;
            }
            let run = &frames[i..j];
            i = j;
            if run.len() < min_frames {
                small += 1;
                continue;
            }
            let seq = &self.seqs[run[0].seq];
            let id = first_id + items.len();
            let name = format!("mpeg_{id:05}_{:012x}.mpg", run[0].start);
            let path = out.join(&name);
            let mut f = std::io::BufWriter::new(std::fs::File::create(&path)?);
            let (mut pieces, mut keys) = (1, 0);
            for (k, fr) in run.iter().enumerate() {
                let data = src.read_vec(fr.start, (fr.end - fr.start) as usize)?;
                if k == 0 && !data.starts_with(&[0, 0, 1, 0xB3]) {
                    f.write_all(&seq.header)?;
                }
                f.write_all(&data)?;
                keys += fr.key as usize;
                if k > 0 && fr.start != run[k - 1].end {
                    pieces += 1;
                }
            }
            f.flush()?;
            drop(f);
            let rate = fps.unwrap_or(RATES[seq.rate as usize]);
            items.push(Recovered {
                id,
                kind: "carved_mpeg".into(),
                output: name,
                sha256: report::sha256_file(&path)?,
                size: std::fs::metadata(&path)?.len(),
                source_start: run[0].start,
                source_end: run[run.len() - 1].end,
                codec: Some(if seq.mpeg2 { "mpeg2video" } else { "mpeg1video" }.into()),
                width: Some(seq.width),
                height: Some(seq.height),
                frames: Some(run.len()),
                keyframes: Some(keys),
                duration_s: Some(run.len() as f64 / rate),
                pieces: Some(pieces),
                status: "ok".into(),
                notes: "MPEG-1/2 video elementary stream, bytes as found; frame rate from the sequence header".into(),
                ..Default::default()
            });
        }
        Ok((items, small))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_header() {
        // 720x576, aspect 2, 25 fps, marker bit set.
        let b = [0x2D, 0x02, 0x40, 0x23, 0x09, 0x27, 0x23, 0x81];
        assert_eq!(parse_seq(&b), Some((720, 576, 3)));
        assert_eq!(parse_seq(&[0u8; 8]), None);
    }
}
