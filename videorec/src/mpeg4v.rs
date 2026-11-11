//! Carving of MPEG-4 Part 2 video (ISO/IEC 14496-2; XviD/DivX and many
//! older DVRs) from raw bytes.
//!
//! Start codes `00 00 01 xx` cannot occur inside coded data in this standard,
//! so a frame (VOP, `00 00 01 B6`) runs to the next start code. A run needs a
//! Video Object Layer header (`00 00 01 20`-`2F`), which gives the picture size
//! and the time resolution used to check each VOP header. Runs start at an
//! I-VOP and are written to AVI (fourcc XVID) with the headers in front of
//! the first frame; frame bytes are not changed.

use crate::avi::AviWriter;
use crate::io::Source;
use crate::report::{self, Recovered};
use anyhow::Result;
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct Vol {
    pub width: u32,
    pub height: u32,
    /// vop_time_increment_resolution and the bit length of vop_time_increment.
    pub res: u32,
    pub inc_bits: u32,
    pub fixed_inc: Option<u32>,
    /// Header bytes (VOS/VO/VOL ...) placed before the first frame.
    pub header: Vec<u8>,
}

struct Bits<'a> {
    b: &'a [u8],
    p: usize,
}
impl Bits<'_> {
    fn get(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.b.get(self.p / 8)?;
            v = (v << 1) | ((byte >> (7 - self.p % 8)) & 1) as u32;
            self.p += 1;
        }
        Some(v)
    }
    fn marker(&mut self) -> Option<()> {
        (self.get(1)? == 1).then_some(())
    }
}

/// Parses a VOL header body (bytes after `00 00 01 2x`), ISO/IEC 14496-2 6.2.3.
pub fn parse_vol(b: &[u8]) -> Option<(u32, u32, u32, u32, Option<u32>)> {
    let mut r = Bits { b, p: 0 };
    r.get(1)?; // random_accessible_vol
    r.get(8)?; // video_object_type_indication
    let verid = if r.get(1)? == 1 {
        let v = r.get(4)?;
        r.get(3)?;
        v
    } else {
        1
    };
    if r.get(4)? == 15 {
        r.get(16)?; // par_width, par_height
    }
    if r.get(1)? == 1 {
        r.get(2)?; // chroma_format
        r.get(1)?; // low_delay
        if r.get(1)? == 1 {
            // vbv_parameters: 15+1+15+1+15+1+3+11+1+15+1
            for n in [15, 1, 15, 1, 15, 1, 3, 11, 1, 15, 1] {
                r.get(n)?;
            }
        }
    }
    let shape = r.get(2)?;
    if shape != 0 {
        return None; // only rectangular video
    }
    let _ = verid;
    r.marker()?;
    let res = r.get(16)?;
    if res == 0 {
        return None;
    }
    r.marker()?;
    let inc_bits = (32 - (res - 1).leading_zeros()).max(1);
    let fixed = if r.get(1)? == 1 {
        Some(r.get(inc_bits)?)
    } else {
        None
    };
    r.marker()?;
    let w = r.get(13)?;
    r.marker()?;
    let h = r.get(13)?;
    r.marker()?;
    if !(16..=4096).contains(&w) || !(16..=4096).contains(&h) {
        return None;
    }
    Some((w, h, res, inc_bits, fixed.filter(|&f| f > 0)))
}

/// Checks a VOP header (bytes after `00 00 01 B6`); returns the coding type.
fn vop_type(b: &[u8], vol: &Vol) -> Option<u32> {
    let mut r = Bits { b, p: 0 };
    let t = r.get(2)?;
    let mut n = 0;
    while r.get(1)? == 1 {
        n += 1;
        if n > 60 {
            return None;
        }
    }
    r.marker()?;
    let inc = r.get(vol.inc_bits)?;
    if inc >= vol.res {
        return None;
    }
    r.marker()?;
    Some(t)
}

#[derive(Debug, Clone)]
struct Frame {
    start: u64,
    end: u64,
    key: bool,
    vol: usize,
}

const MAX_FRAME: u64 = 2 << 20;

/// Removes what follows the frame when it was stored in a container: the
/// next AVI chunk header (`00dc`, `01wb`, `ix00`, `JUNK`, `LIST`, `idx1` with
/// its size) and the AVI pad byte, and zero bytes before the next start code.
/// Decoders treat such bytes as damaged picture data.
fn trim_container_tail(d: &mut Vec<u8>) {
    // The last frame of an AVI runs into its index.
    if let Some(p) = memchr::memmem::find(d, b"idx1") {
        d.truncate(p);
    }
    while d.len() > 4 && d.last() == Some(&0) {
        d.pop();
    }
    let n = d.len();
    for back in 4..=12.min(n) {
        let p = n - back;
        let id = &d[p..p + 4];
        let chunk = id[0].is_ascii_alphanumeric()
            && id[1].is_ascii_alphanumeric()
            && matches!(&id[2..4], b"dc" | b"db" | b"wb" | b"tx");
        let other = matches!(id, b"JUNK" | b"LIST" | b"idx1") || &id[..2] == b"ix";
        if (chunk || other) && back <= 8 {
            d.truncate(p);
            if d.len() % 2 == 1 && d.last() == Some(&0) {
                d.pop();
            }
            while d.len() > 4 && d.last() == Some(&0) {
                d.pop();
            }
            return;
        }
    }
}

/// Finds VOPs in `[start, end)`. Each frame runs to the next start code.
/// Scanner state, kept between consecutive windows of one region.
#[derive(Default)]
pub struct Scanner {
    vols: Vec<Vol>,
    frames: Vec<Frame>,
    cur_vol: Option<usize>,
    /// Header bytes (VOS, VO, VOL, GOV) seen just before the next VOP.
    pending_hdr: Option<u64>,
    open: Option<(u64, bool, usize)>, // frame start, key, vol
    last_code: u64,
    /// Where the previous window stopped and where its region ends.
    at: u64,
    region_end: u64,
}

impl Scanner {
    /// Ends the region being scanned: closes the open frame.
    fn close(&mut self) {
        if let Some((fs, key, v)) = self.open.take() {
            let e = (fs + MAX_FRAME).min(self.region_end);
            self.frames.push(Frame {
                start: fs,
                end: e,
                key,
                vol: v,
            });
        }
        self.cur_vol = None;
        self.pending_hdr = None;
        self.last_code = 0;
    }

    /// Scans `[start, stop)` of a region ending at `end`. A call that starts
    /// where the previous one stopped continues the same region.
    pub fn scan_window(&mut self, src: &Source, start: u64, stop: u64, end: u64) -> Result<()> {
        if start != self.at {
            self.close();
        }
        self.region_end = end;
        let r = scan(src, start, stop, end, self);
        self.at = stop;
        if stop >= end {
            self.close();
            self.at = u64::MAX;
        }
        r
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

    /// Drops frames that start inside `claimed` (found to be something else).
    pub fn exclude(&mut self, claimed: &crate::ranges::Ranges) {
        self.close();
        self.frames
            .retain(|f| claimed.containing(f.start).is_none());
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
        write_runs(
            src,
            &self.vols,
            &self.frames,
            out,
            first_id,
            max_gap,
            min_frames,
            fps,
        )
    }
}

fn scan(src: &Source, start: u64, stop: u64, end: u64, st: &mut Scanner) -> Result<()> {
    const BLOCK: usize = 16 << 20;
    let finder = memchr::memmem::Finder::new(&[0, 0, 1]);
    let mut pos = start;
    let Scanner {
        vols,
        frames,
        cur_vol,
        pending_hdr,
        open,
        last_code,
        ..
    } = st;
    while pos < stop {
        crate::progress::at(pos);
        if crate::progress::stopped() {
            break;
        }
        let want = (BLOCK + 64).min((end - pos) as usize);
        let buf = src.read_vec(pos, want)?;
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
            let code = buf[k + 3];
            let is_vop = code == 0xB6;
            let is_hdr = matches!(code, 0xB0 | 0xB2 | 0xB3 | 0xB5) || code <= 0x2F;
            // Close the open frame at any start code.
            if let Some((fs, key, v)) = open.take()
                && at - fs <= MAX_FRAME
            {
                frames.push(Frame {
                    start: fs,
                    end: at,
                    key,
                    vol: v,
                });
            }
            if at - *last_code > MAX_FRAME {
                *pending_hdr = None;
            }
            *last_code = at;
            if (0x20..=0x2F).contains(&code) {
                let body = &buf[k + 4..(k + 64).min(buf.len())];
                if let Some((w, h, res, inc_bits, fixed)) = parse_vol(body) {
                    let hs = pending_hdr.unwrap_or(at);
                    let v = Vol {
                        width: w,
                        height: h,
                        res,
                        inc_bits,
                        fixed_inc: fixed,
                        header: vec![],
                    };
                    let idx = match vols
                        .iter()
                        .position(|x| (x.width, x.height, x.res, x.fixed_inc) == (w, h, res, fixed))
                    {
                        Some(i) => i,
                        None => {
                            // Header bytes up to the VOL's end are filled in below.
                            let mut v = v;
                            // Up to the next start code after the VOL.
                            let vol_end = memchr::memmem::find(&buf[k + 4..], &[0, 0, 1])
                                .map_or(at + 4 + body.len() as u64, |n| at + 4 + n as u64);
                            v.header = src.read_vec(hs, (vol_end - hs) as usize)?;
                            vols.push(v);
                            vols.len() - 1
                        }
                    };
                    *cur_vol = Some(idx);
                }
                pending_hdr.get_or_insert(at);
                continue;
            }
            if is_hdr {
                pending_hdr.get_or_insert(at);
                continue;
            }
            if is_vop
                && let Some(v) = *cur_vol
                && let Some(t) = vop_type(&buf[k + 4..(k + 16).min(buf.len())], &vols[v])
            {
                let fs = pending_hdr.take().unwrap_or(at);
                *open = Some((fs, t == 0, v));
                continue;
            }
            *pending_hdr = None;
        }
        pos += lim as u64;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_runs(
    src: &Source,
    vols: &[Vol],
    frames: &[Frame],
    out: &Path,
    first_id: usize,
    max_gap: u64,
    min_frames: usize,
    fps: Option<f64>,
) -> Result<(Vec<Recovered>, usize)> {
    let mut items = vec![];
    let mut small = 0;
    let mut i = 0;
    while i < frames.len() {
        // A run starts at an I-VOP.
        if !frames[i].key {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < frames.len()
            && frames[j].vol == frames[i].vol
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
        let vol = &vols[run[0].vol];
        let (rate, scale, assumed) = match (fps, vol.fixed_inc) {
            (Some(f), _) => ((f * 1000.0).round() as u32, 1000, false),
            (None, Some(inc)) => (vol.res, inc, false),
            (None, None) => (25, 1, true),
        };
        let id = first_id + items.len();
        let name = format!("mpeg4_{id:05}_{:012x}.avi", run[0].start);
        let path = out.join(&name);
        let mut w = AviWriter::create(&path, *b"XVID", vol.width, vol.height, rate, scale)?;
        let mut pieces = 1;
        let mut keys = 0;
        for (k, f) in run.iter().enumerate() {
            let mut data = src.read_vec(f.start, (f.end - f.start) as usize)?;
            trim_container_tail(&mut data);
            if k == 0
                && !data
                    .windows(4)
                    .any(|x| x[..3] == [0, 0, 1] && (0x20..=0x2F).contains(&x[3]))
            {
                let mut d = vol.header.clone();
                d.extend_from_slice(&data);
                data = d;
            }
            keys += f.key as usize;
            w.add_frame(&data, f.key)?;
            if k > 0 && f.start != run[k - 1].end {
                pieces += 1;
            }
        }
        w.finish()?;
        let fr = rate as f64 / scale as f64;
        items.push(Recovered {
            id,
            kind: "carved_mpeg4".into(),
            output: name,
            sha256: report::sha256_file(&path)?,
            size: std::fs::metadata(&path)?.len(),
            source_start: run[0].start,
            source_end: run[run.len() - 1].end,
            codec: Some("mpeg4".into()),
            width: Some(vol.width),
            height: Some(vol.height),
            frames: Some(run.len()),
            keyframes: Some(keys),
            duration_s: Some(run.len() as f64 / fr),
            pieces: Some(pieces),
            status: "ok".into(),
            notes: if assumed {
                "MPEG-4 Part 2; frame rate not in the stream, 25 fps assumed (use --fps)".into()
            } else {
                "MPEG-4 Part 2".into()
            },
            ..Default::default()
        });
    }
    Ok((items, small))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vol_from_ffmpeg() {
        // VOL body written by ffmpeg's mpeg4 encoder (-f m4v) for 320x240 at 25 fps.
        let b = [
            0x00, 0xc4, 0x8d, 0x88, 0x00, 0xcd, 0x0a, 0x04, 0x1e, 0x14, 0x43,
        ];
        let v = parse_vol(&b);
        assert!(v.is_some(), "{v:?}");
        let (w, h, res, _, _) = v.unwrap();
        assert_eq!((w, h, res), (320, 240, 25));
    }
}
