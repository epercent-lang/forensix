//! One read of the source for all carving targets.
//!
//! The container scan, the H.264/H.265 carver, the Motion-JPEG scan and the
//! MPEG-4 Part 2 scan used to read the scanned regions one after another:
//! four full reads. Here they move over the source together, a step at a
//! time, sharing a cache of recently read blocks (`Source::set_block_cache`),
//! and are kept in the order that makes the result the same as before:
//!
//! * the container scan runs ahead by `LEAD`, further than the carver ever
//!   looks while testing a chain, so the carver knows every intact file in
//!   the data it examines. An established stream is followed only up to that
//!   point in one call (`Carver::soft_end`) and continued in the next;
//! * the Motion-JPEG and MPEG-4 scans stay behind the first stream the
//!   carver has not finished, so they skip exactly what it claimed.
//!
//! One case cannot be handled in a single read: parameter sets (avcC/hvcC)
//! found on the disk after the carver has already used the list of known
//! parameter sets for a decision. Then the carver's results are dropped, the
//! container scan is finished alone, and `pass` is run a second time without
//! it (two reads in total).

use crate::container;
use crate::io::Source;
use crate::mjpeg;
use crate::mp4::VideoConfig;
use crate::mpeg4v;
use crate::progress;
use crate::ranges::Ranges;
use crate::report::{self, Recovered};
use crate::stream::{self, Carver};
use anyhow::Result;
use std::cell::Cell;
use std::path::Path;
use std::rc::Rc;

/// How far the container scan runs ahead of the carver: more than a chain
/// under test can span, plus the units that may straddle the limit.
const LEAD: u64 = stream::GENERIC_MAX_BYTES + 3 * stream::MAX_NAL;

#[derive(Clone, Copy)]
pub struct Want {
    pub containers: bool,
    pub h26x: bool,
    pub mjpeg: bool,
    pub mpeg4: bool,
}

pub struct Params<'a> {
    pub src: &'a Source,
    pub out: &'a Path,
    pub regions: &'a [(u64, u64)],
    pub opts: &'a stream::Options,
    pub hints: &'a [stream::Hint],
    pub ids: Rc<Cell<usize>>,
    pub quiet: bool,
}

#[derive(Default)]
pub struct Out {
    pub items: Vec<Recovered>,
    pub damaged_containers: usize,
    pub salvaged_containers: usize,
    pub discarded_small: usize,
    pub chains_without_params: usize,
    pub mjpeg_small: usize,
    pub mpeg4_small: usize,
    /// Carving has to be repeated (see the module comment).
    pub redo: bool,
}

/// Step of the walk in bytes (`VIDEOREC_STEP_MB`, default 32 MiB).
fn step() -> u64 {
    std::env::var("VIDEOREC_STEP_MB")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(32)
        << 20
}

fn container_item(
    src: &Source,
    out: &Path,
    id: usize,
    c: &container::Container,
) -> Result<Recovered> {
    let path = out.join(format!("carved_{id:05}_{:012x}.{}", c.start, c.kind));
    crate::copy_range(src, c.start, c.end, &path)?;
    let info = c.config.as_ref().and_then(|x| x.sps_info());
    Ok(Recovered {
        id,
        kind: "carved_container".into(),
        output: path.file_name().unwrap().to_string_lossy().into(),
        sha256: report::sha256_file(&path)?,
        size: c.end - c.start,
        source_start: c.start,
        source_end: c.end,
        codec: c.config.as_ref().map(|x| x.codec.name().into()),
        width: info.as_ref().map(|i| i.width),
        height: info.as_ref().map(|i| i.height),
        duration_s: c.duration_s,
        created: c.created.clone(),
        status: "ok".into(),
        notes: if c.created.is_some() {
            "container structure verified; creation time from the MP4 header (recorder clock)"
                .into()
        } else {
            "container structure verified".into()
        },
        ..Default::default()
    })
}

fn salvaged_item(
    src: &Source,
    out: &Path,
    id: usize,
    c: &container::Container,
    fix: &crate::mp4fix::Fix,
    top: &[crate::mp4::BoxHit],
) -> Result<Recovered> {
    let path = out.join(format!("carved_{id:05}_{:012x}_part.mp4", c.start));
    crate::mp4fix::write(src, c.start, top, fix, &path)?;
    let info = c.config.as_ref().and_then(|x| x.sps_info());
    Ok(Recovered {
        id,
        kind: "carved_container".into(),
        output: path.file_name().unwrap().to_string_lossy().into(),
        sha256: report::sha256_file(&path)?,
        size: std::fs::metadata(&path)?.len(),
        source_start: c.start,
        source_end: c.start + fix.cutoff,
        codec: c.config.as_ref().map(|x| x.codec.name().into()),
        width: info.as_ref().map(|i| i.width),
        height: info.as_ref().map(|i| i.height),
        frames: Some(fix.video_kept),
        duration_s: Some(fix.duration_s),
        created: c.created.clone(),
        status: "partial".into(),
        notes: format!(
            "MP4 with intact index but data cut off or overwritten: kept the first {} of {} video frames with {} track(s), {} of them audio, and the recorded timing; index rewritten for the kept part ({})",
            fix.video_kept, fix.video_total, fix.tracks, fix.audio_tracks, c.note
        ),
        ..Default::default()
    })
}

/// Scans the parts of `regions` not in `covered`. `claimed` lists what the
/// items found earlier occupy; `covered` and `configs` are added to.
pub fn pass(
    p: &Params,
    want: Want,
    covered: &mut Ranges,
    claimed: &[(u64, u64)],
    configs: &mut Vec<VideoConfig>,
) -> Result<Out> {
    let (src, out) = (p.src, p.out);
    let len = src.len();
    let step = step();
    let mut o = Out::default();
    covered.normalize();
    let gaps: Vec<(u64, u64)> = p
        .regions
        .iter()
        .flat_map(|&(a, b)| covered.gaps(a, b))
        .collect();
    // What the scans behind the carver must skip: everything outside the
    // gaps, what earlier items occupy, and what is found on the way.
    let mut fixed = Ranges::new();
    let mut prev = 0;
    for &(s, e) in &gaps {
        fixed.add(prev, s);
        prev = prev.max(e);
    }
    fixed.add(prev, len);
    for &(a, b) in claimed {
        fixed.add(a, b);
    }
    fixed.normalize();
    let mut taken = Taken {
        fixed,
        found: Ranges::new(),
    };

    let mut carver = want
        .h26x
        .then(|| Carver::new(src, p.opts, configs.clone(), p.hints, out, p.ids.clone()));
    let mut known_configs = configs.len();
    let mut seen = 0usize; // carver results already in `taken`
    let mut lag = want.mjpeg || want.mpeg4;
    let mut jpegs: Vec<mjpeg::Jpeg> = vec![];
    let mut vops = mpeg4v::Scanner::default();
    let (mut jpos, mut mpos) = (0u64, 0u64);
    let mut containers: Vec<Recovered> = vec![];

    progress::stage(
        if want.containers && !(want.h26x || lag) {
            "files"
        } else {
            "video"
        },
        gaps.iter().map(|g| g.1 - g.0).sum(),
        p.quiet,
    );
    let mut done = 0u64;
    'gaps: for &(s, e) in &gaps {
        progress::region(s, done);
        done += e - s;
        let (mut cpos, mut kpos, mut stop) = (s, s, s);
        while stop < e {
            if progress::stopped() {
                break 'gaps;
            }
            stop = e.min((stop / step + 1) * step);
            // 1. Intact files, ahead of everything else.
            let mut frontier = e;
            if want.containers {
                let f = e.min(stop + LEAD);
                if cpos < f {
                    let mut found = |c: container::Container| -> Result<u64> {
                        if let (false, Some((fix, top))) = (c.intact, &c.fix) {
                            // Index intact, data cut off: keep the part
                            // before the damage with all its tracks.
                            let id = p.ids.get();
                            p.ids.set(id + 1);
                            let end = c.start + fix.cutoff;
                            containers.push(salvaged_item(src, out, id, &c, fix, top)?);
                            o.salvaged_containers += 1;
                            covered.insert(c.start, end);
                            taken.found.insert(c.start, end);
                            return Ok(end);
                        }
                        if !c.intact {
                            o.damaged_containers += 1;
                            return Ok(0);
                        }
                        let id = p.ids.get();
                        p.ids.set(id + 1);
                        containers.push(container_item(src, out, id, &c)?);
                        covered.insert(c.start, c.end);
                        taken.found.insert(c.start, c.end);
                        Ok(c.end)
                    };
                    cpos = container::scan_window(src, cpos, f, e, configs, &mut found)?;
                }
                frontier = e.min(cpos.max(f));
            }
            if configs.len() > known_configs {
                known_configs = configs.len();
                if let Some(cv) = carver.as_mut() {
                    if cv.consulted {
                        // Too late for what was already decided: start over
                        // after the container scan (see the module comment).
                        for r in &cv.results {
                            std::fs::remove_file(out.join(&r.output)).ok();
                        }
                        carver = None;
                        lag = false;
                        o.redo = true;
                    } else {
                        cv.set_configs(configs.clone());
                    }
                }
            }
            // 2. H.264/H.265 streams in what the files left.
            if let Some(cv) = carver.as_mut() {
                cv.soft_end = if frontier < e { frontier } else { u64::MAX };
                while kpos < stop {
                    let a = covered.containing(kpos).map_or(kpos, |r| r.1);
                    if a >= stop {
                        kpos = a.min(e);
                        break;
                    }
                    let b = covered.next_start(a).min(e);
                    kpos = cv.scan_window(a, stop.min(b), b)?;
                    if progress::stopped() {
                        break 'gaps;
                    }
                }
            } else {
                kpos = stop;
            }
            // 3. Motion-JPEG and MPEG-4 behind the carver.
            if lag {
                let mut limit = frontier.min(kpos);
                if let Some(cv) = &carver {
                    for r in &cv.results[seen..] {
                        taken.found.insert(r.source_start, r.source_end);
                    }
                    seen = cv.results.len();
                    limit = limit.min(cv.undecided_start().unwrap_or(u64::MAX));
                }
                if want.mjpeg {
                    jpos = lag_jpeg(src, &taken, jpos, limit, false, &mut jpegs)?;
                }
                if want.mpeg4 {
                    mpos = lag_mpeg4(src, &taken, mpos, limit, &mut vops)?;
                }
            }
        }
    }
    if let Some(mut cv) = carver.take() {
        cv.flush()?;
        for r in &cv.results[seen..] {
            taken.found.insert(r.source_start, r.source_end);
        }
        o.discarded_small = cv.discarded_small;
        o.chains_without_params = cv.chains_without_params;
        o.items.extend(cv.results);
    }
    o.items.extend(containers);
    if lag && !progress::stopped() {
        if want.mjpeg {
            lag_jpeg(src, &taken, jpos, len, true, &mut jpegs)?;
        }
        if want.mpeg4 {
            lag_mpeg4(src, &taken, mpos, len, &mut vops)?;
        }
    }
    let (max_gap, min_frames, fps) = (p.opts.max_gap, p.opts.min_frames, p.opts.fps);
    if lag && want.mjpeg {
        let (got, small) =
            mjpeg::write_runs(src, &jpegs, out, p.ids.get(), max_gap, min_frames, fps)?;
        p.ids.set(p.ids.get() + got.len());
        o.mjpeg_small = small;
        // The MPEG-4 scan used to skip what became Motion-JPEG clips.
        let mut r = Ranges::new();
        for it in &got {
            r.add(it.source_start, it.source_end);
        }
        r.normalize();
        vops.exclude(&r);
        o.items.extend(got);
    }
    if lag && want.mpeg4 {
        let (got, small) = vops.finish(src, out, p.ids.get(), max_gap, min_frames, fps)?;
        p.ids.set(p.ids.get() + got.len());
        o.mpeg4_small = small;
        o.items.extend(got);
    }
    o.items.sort_by_key(|i| i.id);
    Ok(o)
}

/// What the scans behind the carver skip: `fixed` is known before the pass,
/// `found` grows during it (mostly at its end, so inserting is cheap).
struct Taken {
    fixed: Ranges,
    found: Ranges,
}

impl Taken {
    /// First position at or after `p` that is not taken.
    fn skip(&self, mut p: u64) -> u64 {
        loop {
            if let Some(r) = self.fixed.containing(p) {
                p = r.1;
            } else if let Some(r) = self.found.containing(p) {
                p = r.1;
            } else {
                return p;
            }
        }
    }

    fn next_start(&self, p: u64) -> u64 {
        self.fixed.next_start(p).min(self.found.next_start(p))
    }
}

/// Motion-JPEG scan of what is free in `[pos, limit)`. Unless `last`, images
/// are only looked for where a whole one fits before `limit`, because what
/// lies beyond is not decided yet. Returns the new position.
fn lag_jpeg(
    src: &Source,
    taken: &Taken,
    mut pos: u64,
    limit: u64,
    last: bool,
    found: &mut Vec<mjpeg::Jpeg>,
) -> Result<u64> {
    while pos < limit && !progress::stopped() {
        let a = taken.skip(pos);
        if a >= limit {
            return Ok(limit);
        }
        let b = taken.next_start(a).min(src.len());
        if b <= limit || last {
            mjpeg::scan_window(src, a, b, b, found)?;
            pos = b;
        } else {
            let stop = limit.saturating_sub(mjpeg::MAX_JPEG as u64);
            if stop <= a {
                return Ok(a);
            }
            return mjpeg::scan_window(src, a, stop, limit, found);
        }
    }
    Ok(pos.max(limit))
}

/// MPEG-4 Part 2 scan of what is free in `[pos, limit)`.
fn lag_mpeg4(
    src: &Source,
    taken: &Taken,
    mut pos: u64,
    limit: u64,
    sc: &mut mpeg4v::Scanner,
) -> Result<u64> {
    while pos < limit && !progress::stopped() {
        let a = taken.skip(pos);
        if a > pos {
            sc.end_region(pos);
        }
        if a >= limit {
            return Ok(limit);
        }
        let b = taken.next_start(a).min(src.len());
        let stop = b.min(limit);
        sc.scan_window(src, a, stop, b)?;
        pos = stop;
    }
    Ok(pos.max(limit))
}
