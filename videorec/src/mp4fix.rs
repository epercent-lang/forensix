//! Salvage of an MP4 whose index (moov) is intact but whose data is cut off
//! or overwritten from some point on.
//!
//! Such a file used to be left to frame carving, which keeps the video frames
//! but loses the sound and the recorded timing. Here the sample tables are
//! shortened to the samples that are still there, so the part before the
//! damage plays as the recorder wrote it: all tracks, original timestamps.
//!
//! Box layout and table formats: ISO/IEC 14496-12 (stbl 8.5-8.7, mvhd 8.2.2,
//! tkhd 8.3.2, mdhd 8.4.2). Only what the tables describe is trusted: a video
//! sample counts as present when it lies inside the readable area and is an
//! exact chain of length-prefixed NAL units of the track's codec.

use crate::codec::{self, Codec};
use crate::io::{Source, be32, be64};
use crate::mp4::{self, BoxHit};
use anyhow::Result;
use std::io::Write;
use std::path::Path;

/// Fewer verified video samples than this are left to frame carving.
const MIN_VIDEO_SAMPLES: usize = 15;
const MAX_SAMPLES: usize = 50_000_000;

#[derive(Debug, Clone)]
pub struct Fix {
    /// End of the kept data, relative to the file start.
    pub cutoff: u64,
    /// Complete new `moov` box.
    moov: Vec<u8>,
    /// Size of the `mdat` header to write (8 or 16).
    mdat_hdr: u64,
    pub video_kept: usize,
    pub video_total: usize,
    pub tracks: usize,
    pub audio_tracks: usize,
    pub duration_s: f64,
}

fn boxes(b: &[u8]) -> Option<Vec<([u8; 4], &[u8])>> {
    let mut out = vec![];
    let mut p = 0;
    while p + 8 <= b.len() {
        let size = be32(b, p) as usize;
        if size < 8 || p + size > b.len() {
            return None; // 64-bit or broken sizes inside moov: not handled
        }
        out.push((b[p + 4..p + 8].try_into().unwrap(), &b[p + 8..p + size]));
        p += size;
    }
    (p == b.len()).then_some(out)
}

fn mk(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(payload.len() + 8);
    v.extend_from_slice(&(payload.len() as u32 + 8).to_be_bytes());
    v.extend_from_slice(typ);
    v.extend_from_slice(payload);
    v
}

fn find<'a>(bs: &[([u8; 4], &'a [u8])], typ: &[u8; 4]) -> Option<&'a [u8]> {
    bs.iter().find(|b| &b.0 == typ).map(|b| b.1)
}

/// Run-length table (stts, ctts): `(count, value)` pairs after an 8-byte head.
fn runs(b: &[u8]) -> Option<Vec<(u32, u32)>> {
    let n = be32(b.get(..8)?, 4) as usize;
    let body = b.get(8..8 + n.checked_mul(8)?)?;
    Some(
        body.chunks_exact(8)
            .map(|c| (be32(c, 0), be32(c, 4)))
            .collect(),
    )
}

fn cut_runs(r: &[(u32, u32)], mut n: usize) -> (Vec<(u32, u32)>, u64) {
    let mut out = vec![];
    let mut sum = 0u64;
    for &(c, v) in r {
        if n == 0 {
            break;
        }
        let k = (c as usize).min(n);
        out.push((k as u32, v));
        sum += k as u64 * v as u64;
        n -= k;
    }
    (out, sum)
}

fn runs_box(typ: &[u8; 4], head: &[u8], r: &[(u32, u32)]) -> Vec<u8> {
    let mut p = head[..4].to_vec();
    p.extend_from_slice(&(r.len() as u32).to_be_bytes());
    for &(c, v) in r {
        p.extend_from_slice(&c.to_be_bytes());
        p.extend_from_slice(&v.to_be_bytes());
    }
    mk(typ, &p)
}

struct Track<'a> {
    stbl: Vec<([u8; 4], &'a [u8])>,
    handler: [u8; 4],
    timescale: u32,
    /// (offset relative to file start, size) per sample.
    samples: Vec<(u64, u32)>,
    /// Samples per chunk and description index, per chunk.
    chunks: Vec<(u64, u32, u32)>, // offset, samples, description
    video: Option<(Codec, usize)>,
    keep: usize,
}

fn track<'a>(trak: &'a [u8]) -> Option<Track<'a>> {
    let t = boxes(trak)?;
    let mdia = boxes(find(&t, b"mdia")?)?;
    let hdlr = find(&mdia, b"hdlr")?;
    let handler: [u8; 4] = hdlr.get(8..12)?.try_into().ok()?;
    let mdhd = find(&mdia, b"mdhd")?;
    let timescale = match *mdhd.first()? {
        1 => be32(mdhd.get(..24)?, 20),
        _ => be32(mdhd.get(..16)?, 12),
    };
    let minf = boxes(find(&mdia, b"minf")?)?;
    let stbl = boxes(find(&minf, b"stbl")?)?;
    let stsz = find(&stbl, b"stsz")?;
    let (uniform, n) = (be32(stsz.get(..12)?, 4), be32(stsz, 8) as usize);
    if n > MAX_SAMPLES || (uniform == 0 && stsz.len() < 12 + n * 4) {
        return None;
    }
    let size = |i: usize| {
        if uniform != 0 {
            uniform
        } else {
            be32(stsz, 12 + i * 4)
        }
    };
    let offs: Vec<u64> = if let Some(b) = find(&stbl, b"stco") {
        let c = be32(b.get(..8)?, 4) as usize;
        b.get(8..8 + c.checked_mul(4)?)?
            .chunks_exact(4)
            .map(|x| be32(x, 0) as u64)
            .collect()
    } else {
        let b = find(&stbl, b"co64")?;
        let c = be32(b.get(..8)?, 4) as usize;
        b.get(8..8 + c.checked_mul(8)?)?
            .chunks_exact(8)
            .map(|x| be64(x, 0))
            .collect()
    };
    let stsc = find(&stbl, b"stsc")?;
    let ne = be32(stsc.get(..8)?, 4) as usize;
    let e: Vec<(u32, u32, u32)> = stsc
        .get(8..8 + ne.checked_mul(12)?)?
        .chunks_exact(12)
        .map(|x| (be32(x, 0), be32(x, 4), be32(x, 8)))
        .collect();
    let mut samples = Vec::with_capacity(n);
    let mut chunks = Vec::with_capacity(offs.len());
    let mut k = 0;
    for (ci, &co) in offs.iter().enumerate() {
        while k + 1 < e.len() && e[k + 1].0 as usize <= ci + 1 {
            k += 1;
        }
        let &(first, per, desc) = e.get(k)?;
        if first as usize > ci + 1 {
            return None;
        }
        let mut o = co;
        let mut got = 0;
        for _ in 0..per {
            if samples.len() >= n {
                break;
            }
            let s = size(samples.len());
            samples.push((o, s));
            o += s as u64;
            got += 1;
        }
        chunks.push((co, got, desc));
    }
    // What the video track is coded with, for checking its samples.
    let mut video = None;
    if &handler == b"vide"
        && let Some(stsd) = find(&stbl, b"stsd")
        && stsd.len() > 8
        && let Some(entries) = boxes(&stsd[8..])
    {
        for (et, entry) in entries {
            if entry.len() < 78 {
                continue;
            }
            let Some(sub) = boxes(&entry[78..]) else {
                continue;
            };
            let cfg = match &et {
                b"avc1" | b"avc3" => find(&sub, b"avcC").and_then(mp4::parse_avcc),
                b"hvc1" | b"hev1" => find(&sub, b"hvcC").and_then(mp4::parse_hvcc),
                _ => None,
            };
            if let Some(c) = cfg {
                video = Some((c.codec, c.length_size as usize));
                break;
            }
        }
    }
    Some(Track {
        stbl,
        handler,
        timescale,
        samples,
        chunks,
        video,
        keep: 0,
    })
}

/// True if the sample is an exact chain of length-prefixed NAL units.
fn sample_ok(src: &Source, abs: u64, size: u32, c: Codec, ls: usize) -> Result<bool> {
    let end = abs + size as u64;
    let mut p = abs;
    let mut b = [0u8; 8];
    while p < end {
        if p + ls as u64 + 2 > end || src.read_at(p, &mut b[..ls + 2])? < ls + 2 {
            return Ok(false);
        }
        let len = b[..ls].iter().fold(0u64, |a, &x| (a << 8) | x as u64);
        if len < 2 || p + ls as u64 + len > end || codec::classify(c, &b[ls..ls + 2]).is_none() {
            return Ok(false);
        }
        p += ls as u64 + len;
    }
    Ok(true)
}

fn set_duration(b: &[u8], tkhd: bool, d: u64) -> Vec<u8> {
    let mut v = b.to_vec();
    let v1 = v.first() == Some(&1);
    let at = match (v1, tkhd) {
        (true, false) => 24,
        (true, true) => 28,
        (false, false) => 16,
        (false, true) => 20,
    };
    if v1 && v.len() >= at + 8 {
        v[at..at + 8].copy_from_slice(&d.to_be_bytes());
    } else if !v1 && v.len() >= at + 4 {
        v[at..at + 4].copy_from_slice(&(d.min(u32::MAX as u64) as u32).to_be_bytes());
    }
    v
}

/// New sample table for the first `t.keep` samples; chunk offsets moved by
/// `delta`. Returns the box and the media duration.
fn new_stbl(t: &Track, delta: i64) -> Option<(Vec<u8>, u64)> {
    let n = t.keep;
    let mut out = vec![];
    let mut dur = 0;
    // Chunks that hold the kept samples.
    let mut ch: Vec<(u64, u32, u32)> = vec![];
    let mut left = n as u64;
    for &(o, c, d) in &t.chunks {
        if left == 0 {
            break;
        }
        let k = (c as u64).min(left) as u32;
        ch.push(((o as i64 + delta) as u64, k, d));
        left -= k as u64;
    }
    for (typ, b) in &t.stbl {
        match typ {
            b"stsz" => {
                let mut p = b.get(..8)?.to_vec();
                p.extend_from_slice(&(n as u32).to_be_bytes());
                if be32(b, 4) == 0 {
                    p.extend_from_slice(b.get(12..12 + n * 4)?);
                }
                out.extend(mk(typ, &p));
            }
            b"stco" | b"co64" => {
                let wide = ch.iter().any(|c| c.0 > u32::MAX as u64);
                let mut p = b.get(..4)?.to_vec();
                p.extend_from_slice(&(ch.len() as u32).to_be_bytes());
                for c in &ch {
                    if wide {
                        p.extend_from_slice(&c.0.to_be_bytes());
                    } else {
                        p.extend_from_slice(&(c.0 as u32).to_be_bytes());
                    }
                }
                out.extend(mk(if wide { b"co64" } else { b"stco" }, &p));
            }
            b"stsc" => {
                let mut e: Vec<(u32, u32, u32)> = vec![];
                for (i, c) in ch.iter().enumerate() {
                    if e.last().is_none_or(|l| (l.1, l.2) != (c.1, c.2)) {
                        e.push((i as u32 + 1, c.1, c.2));
                    }
                }
                let mut p = b.get(..4)?.to_vec();
                p.extend_from_slice(&(e.len() as u32).to_be_bytes());
                for x in e {
                    for v in [x.0, x.1, x.2] {
                        p.extend_from_slice(&v.to_be_bytes());
                    }
                }
                out.extend(mk(typ, &p));
            }
            b"stts" => {
                let (r, d) = cut_runs(&runs(b)?, n);
                dur = d;
                out.extend(runs_box(typ, b, &r));
            }
            b"ctts" => {
                let (r, _) = cut_runs(&runs(b)?, n);
                out.extend(runs_box(typ, b, &r));
            }
            b"stss" => {
                let c = be32(b.get(..8)?, 4) as usize;
                let keep: Vec<&[u8]> = b
                    .get(8..8 + c.checked_mul(4)?)?
                    .chunks_exact(4)
                    .filter(|x| be32(x, 0) as usize <= n)
                    .collect();
                let mut p = b[..4].to_vec();
                p.extend_from_slice(&(keep.len() as u32).to_be_bytes());
                for x in keep {
                    p.extend_from_slice(x);
                }
                out.extend(mk(typ, &p));
            }
            // Optional per-sample tables: dropped rather than shortened.
            b"sdtp" | b"sbgp" | b"sgpd" | b"saiz" | b"saio" | b"subs" | b"stdp" | b"padb"
            | b"stsh" | b"stz2" => {}
            _ => out.extend(mk(typ, b)),
        }
    }
    Some((mk(b"stbl", &out), dur))
}

fn rebuild(moov: &[u8], tracks: &[Option<Track>], delta: i64) -> Option<(Vec<u8>, f64)> {
    let top = boxes(moov)?;
    let mvhd = find(&top, b"mvhd")?;
    let movie_ts = match *mvhd.first()? {
        1 => be32(mvhd.get(..24)?, 20),
        _ => be32(mvhd.get(..16)?, 12),
    } as u64;
    let mut out = vec![];
    let mut longest = 0u64;
    let mut ti = 0;
    let mut mvhd_at = None;
    for (typ, b) in &top {
        match typ {
            b"mvhd" => {
                mvhd_at = Some(out.len());
                out.extend(mk(typ, b));
            }
            b"trak" => {
                let t = tracks.get(ti)?.as_ref();
                ti += 1;
                let Some(t) = t.filter(|t| t.keep > 0) else {
                    continue; // nothing of this track is left
                };
                let (stbl, dur) = new_stbl(t, delta)?;
                let movie_dur = if t.timescale > 0 {
                    dur * movie_ts / t.timescale as u64
                } else {
                    0
                };
                longest = longest.max(movie_dur);
                let mut tr = vec![];
                for (ty, tb) in boxes(b)? {
                    match &ty {
                        b"tkhd" => tr.extend(mk(&ty, &set_duration(tb, true, movie_dur))),
                        b"mdia" => {
                            let mut md = vec![];
                            for (my, mb) in boxes(tb)? {
                                match &my {
                                    b"mdhd" => md.extend(mk(&my, &set_duration(mb, false, dur))),
                                    b"minf" => {
                                        let mut mi = vec![];
                                        for (iy, ib) in boxes(mb)? {
                                            if &iy == b"stbl" {
                                                mi.extend(&stbl);
                                            } else {
                                                mi.extend(mk(&iy, ib));
                                            }
                                        }
                                        md.extend(mk(&my, &mi));
                                    }
                                    _ => md.extend(mk(&my, mb)),
                                }
                            }
                            tr.extend(mk(&ty, &md));
                        }
                        _ => tr.extend(mk(&ty, tb)),
                    }
                }
                out.extend(mk(typ, &tr));
            }
            // Fragmented files (mvex/moof) are not handled.
            b"mvex" => return None,
            _ => out.extend(mk(typ, b)),
        }
    }
    let at = mvhd_at?;
    let fixed = set_duration(mvhd, false, longest);
    out[at + 8..at + 8 + fixed.len()].copy_from_slice(&fixed);
    let secs = if movie_ts > 0 {
        longest as f64 / movie_ts as f64
    } else {
        0.0
    };
    Some((mk(b"moov", &out), secs))
}

/// Works out what can be kept of the MP4 at `start` whose top-level boxes
/// are `top` and whose data is readable up to `limit`.
pub fn plan(
    src: &Source,
    start: u64,
    top: &[BoxHit],
    moov: &[u8],
    limit: u64,
) -> Result<Option<Fix>> {
    let mdats: Vec<&BoxHit> = top.iter().filter(|b| &b.typ == b"mdat").collect();
    let (Some(moov_box), [mdat]) = (top.iter().find(|b| &b.typ == b"moov"), mdats.as_slice())
    else {
        return Ok(None);
    };
    let Some(top_moov) = boxes(moov) else {
        return Ok(None);
    };
    let mut tracks: Vec<Option<Track>> = top_moov
        .iter()
        .filter(|b| &b.0 == b"trak")
        .map(|b| track(b.1))
        .collect();
    let mut hdr = [0u8; 16];
    src.read_at(mdat.offset, &mut hdr)?;
    let old_hdr: u64 = if be32(&hdr, 0) == 1 { 16 } else { 8 };
    let data_start = mdat.offset + old_hdr - start;
    let mut bound = limit - start;
    if mdat.size > 0 {
        bound = bound.min(mdat.offset + mdat.size - start);
    }
    // Video: the first sample that is not what the table says ends the file.
    let (mut video_kept, mut video_total, mut checked) = (0usize, 0usize, false);
    // The damage starts somewhere before the first sample that fails the
    // check, possibly inside the sample before it (only sample headers can
    // be checked) or in the sound stored between the two. So that last
    // sample, and everything from its start on, is not kept.
    for t in tracks.iter_mut().flatten() {
        let Some((c, ls)) = t.video else { continue };
        checked = true;
        video_total += t.samples.len();
        let mut prev = None;
        for &(o, s) in &t.samples {
            if o < data_start || o + s as u64 > bound {
                bound = bound.min(o.max(data_start));
                break;
            }
            if !sample_ok(src, start + o, s, c, ls)? {
                bound = bound.min(prev.unwrap_or(o).max(data_start));
                video_kept = video_kept.saturating_sub(1);
                break;
            }
            prev = Some(o);
            video_kept += 1;
        }
    }
    if !checked || video_kept < MIN_VIDEO_SAMPLES {
        return Ok(None);
    }
    let mut cutoff = data_start;
    let (mut n, mut audio) = (0, 0);
    for t in tracks.iter_mut().flatten() {
        t.keep = t
            .samples
            .iter()
            .take_while(|&&(o, s)| o >= data_start && o + s as u64 <= bound)
            .count();
        if let Some(&(o, s)) = t.keep.checked_sub(1).and_then(|k| t.samples.get(k)) {
            cutoff = cutoff.max(o + s as u64);
            n += 1;
            audio += (&t.handler == b"soun") as usize;
        }
    }
    // The new moov replaces the old one; data after it moves by the change.
    let front = moov_box.offset < mdat.offset;
    let body = cutoff - data_start;
    let new_hdr: u64 = if body + old_hdr > u32::MAX as u64 {
        16
    } else {
        old_hdr
    };
    let mut delta = new_hdr as i64 - old_hdr as i64;
    let mut built = None;
    for _ in 0..3 {
        let Some((m, secs)) = rebuild(moov, &tracks, delta) else {
            return Ok(None);
        };
        let d = new_hdr as i64 - old_hdr as i64
            + if front {
                m.len() as i64 - moov_box.size as i64
            } else {
                0
            };
        let stable = d == delta;
        delta = d;
        built = Some((m, secs));
        if stable {
            break;
        }
    }
    let Some((moov, duration_s)) = built else {
        return Ok(None);
    };
    Ok(Some(Fix {
        cutoff,
        moov,
        mdat_hdr: new_hdr,
        video_kept,
        video_total,
        tracks: n,
        audio_tracks: audio,
        duration_s,
    }))
}

/// Writes the salvaged file: the boxes before the data as they were, the
/// shortened index in place of the old one, and the data up to the cut.
pub fn write(src: &Source, start: u64, top: &[BoxHit], fix: &Fix, path: &Path) -> Result<()> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    let copy = |f: &mut std::io::BufWriter<std::fs::File>, mut a: u64, b: u64| -> Result<()> {
        let mut buf = vec![0u8; 4 << 20];
        while a < b {
            let n = src.read_at(a, &mut buf[..((b - a) as usize).min(4 << 20)])?;
            if n == 0 {
                anyhow::bail!("short read at {a:#x}");
            }
            f.write_all(&buf[..n])?;
            a += n as u64;
        }
        Ok(())
    };
    let mut moov_done = false;
    for b in top {
        match &b.typ {
            b"moov" => {
                f.write_all(&fix.moov)?;
                moov_done = true;
            }
            b"mdat" => {
                let mut hdr = [0u8; 16];
                src.read_at(b.offset, &mut hdr)?;
                let old_hdr: u64 = if be32(&hdr, 0) == 1 { 16 } else { 8 };
                let data = b.offset + old_hdr;
                let body = start + fix.cutoff - data;
                if fix.mdat_hdr == 16 {
                    f.write_all(&1u32.to_be_bytes())?;
                    f.write_all(b"mdat")?;
                    f.write_all(&(body + 16).to_be_bytes())?;
                } else {
                    f.write_all(&((body + 8) as u32).to_be_bytes())?;
                    f.write_all(b"mdat")?;
                }
                copy(&mut f, data, start + fix.cutoff)?;
                if !moov_done {
                    f.write_all(&fix.moov)?;
                }
                break;
            }
            _ => copy(&mut f, b.offset, b.offset + b.size)?,
        }
    }
    f.flush()?;
    Ok(())
}
