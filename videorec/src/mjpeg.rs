//! Motion-JPEG carving: runs of complete JPEG images (ITU-T T.81 / JFIF)
//! with the same picture size, stored close together, are joined into one
//! MJPEG AVI. Single photos are dropped by the minimum frame count.

use crate::avi::AviWriter;
use crate::io::Source;
use crate::report::{self, Recovered};
use anyhow::Result;
use std::path::Path;

const MAX_JPEG: usize = 8 << 20;

#[derive(Debug, Clone, Copy)]
pub struct Jpeg {
    pub start: u64,
    pub end: u64,
    pub width: u16,
    pub height: u16,
}

/// Parses a JPEG starting at `b[0]` (FF D8). Returns (length, width, height)
/// when the marker structure is valid up to EOI.
pub fn parse(b: &[u8]) -> Option<(usize, u16, u16)> {
    if b.len() < 4 || b[0] != 0xFF || b[1] != 0xD8 || b[2] != 0xFF {
        return None;
    }
    let mut i = 2;
    let (mut w, mut h) = (0u16, 0u16);
    let (mut dqt, mut sos) = (false, false);
    loop {
        // Skip fill bytes.
        while i < b.len() && b[i] == 0xFF && i + 1 < b.len() && b[i + 1] == 0xFF {
            i += 1;
        }
        if i + 1 >= b.len() || b[i] != 0xFF {
            return None;
        }
        let m = b[i + 1];
        i += 2;
        match m {
            0xD9 => {
                return (sos && dqt && w > 0 && h > 0).then_some((i, w, h));
            }
            0x01 | 0xD0..=0xD7 => continue,
            0x00 | 0xD8 => return None,
            _ => {}
        }
        if i + 2 > b.len() {
            return None;
        }
        let len = u16::from_be_bytes([b[i], b[i + 1]]) as usize;
        if len < 2 || i + len > b.len() {
            return None;
        }
        match m {
            0xDB => dqt = true,
            0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => {
                if len < 8 {
                    return None;
                }
                h = u16::from_be_bytes([b[i + 3], b[i + 4]]);
                w = u16::from_be_bytes([b[i + 5], b[i + 6]]);
            }
            0xDA => {
                if w == 0 {
                    return None;
                }
                sos = true;
                // Entropy-coded data runs to the next marker other than
                // stuffed 00 and restart markers.
                let mut j = i + len;
                loop {
                    let k = memchr::memchr(0xFF, &b[j..])? + j;
                    if k + 1 >= b.len() {
                        return None;
                    }
                    match b[k + 1] {
                        0x00 | 0xD0..=0xD7 | 0xFF => j = k + 1,
                        _ => {
                            j = k;
                            break;
                        }
                    }
                }
                i = j;
                continue;
            }
            _ => {}
        }
        i += len;
    }
}

/// Finds complete JPEG images in `[start, end)`.
pub fn scan(src: &Source, start: u64, end: u64, found: &mut Vec<Jpeg>) -> Result<()> {
    const BLOCK: usize = 16 << 20;
    let finder = memchr::memmem::Finder::new(&[0xFF, 0xD8, 0xFF]);
    let mut pos = start;
    let mut buf = vec![0u8; BLOCK + MAX_JPEG];
    while pos < end {
        let want = (BLOCK + MAX_JPEG).min((end - pos) as usize);
        let n = src.read_at(pos, &mut buf[..want])?;
        if n < 4 {
            break;
        }
        let lim = if n < want { n } else { n.min(BLOCK) };
        let mut i = 0;
        let mut next = pos + lim as u64;
        while let Some(k) = finder.find(&buf[i..lim]) {
            let at = i + k;
            if let Some((len, w, h)) = parse(&buf[at..n]) {
                found.push(Jpeg {
                    start: pos + at as u64,
                    end: pos + (at + len) as u64,
                    width: w,
                    height: h,
                });
                if at + len >= lim {
                    next = pos + (at + len) as u64;
                    break;
                }
                i = at + len;
            } else {
                i = at + 1;
            }
        }
        pos = next.max(pos + 1);
    }
    Ok(())
}

/// Joins images of the same size whose gaps are at most `max_gap` bytes and
/// writes each run of at least `min_frames` as an MJPEG AVI.
pub fn write_runs(
    src: &Source,
    jpegs: &[Jpeg],
    out: &Path,
    first_id: usize,
    max_gap: u64,
    min_frames: usize,
    fps: Option<f64>,
) -> Result<(Vec<Recovered>, usize)> {
    let mut items = vec![];
    let mut small = 0;
    let mut i = 0;
    while i < jpegs.len() {
        let mut j = i + 1;
        while j < jpegs.len()
            && (jpegs[j].width, jpegs[j].height) == (jpegs[i].width, jpegs[i].height)
            && jpegs[j].start.saturating_sub(jpegs[j - 1].end) <= max_gap
        {
            j += 1;
        }
        let run = &jpegs[i..j];
        i = j;
        if run.len() < min_frames {
            small += 1;
            continue;
        }
        let id = first_id + items.len();
        let (rate, scale, assumed) = match fps {
            Some(f) => ((f * 1000.0).round() as u32, 1000, false),
            None => (10, 1, true),
        };
        let name = format!("mjpeg_{id:05}_{:012x}.avi", run[0].start);
        let path = out.join(&name);
        let mut w = AviWriter::create(
            &path,
            *b"MJPG",
            run[0].width as u32,
            run[0].height as u32,
            rate,
            scale,
        )?;
        let mut pieces = 1;
        for (k, f) in run.iter().enumerate() {
            w.add_frame(&src.read_vec(f.start, (f.end - f.start) as usize)?, true)?;
            if k > 0 && f.start != run[k - 1].end {
                pieces += 1;
            }
        }
        w.finish()?;
        let fr = rate as f64 / scale as f64;
        items.push(Recovered {
            id,
            kind: "carved_mjpeg".into(),
            output: name,
            sha256: report::sha256_file(&path)?,
            size: std::fs::metadata(&path)?.len(),
            source_start: run[0].start,
            source_end: run[run.len() - 1].end,
            codec: Some("mjpeg".into()),
            width: Some(run[0].width as u32),
            height: Some(run[0].height as u32),
            frames: Some(run.len()),
            keyframes: Some(run.len()),
            duration_s: Some(run.len() as f64 / fr),
            pieces: Some(pieces),
            status: "ok".into(),
            notes: if assumed {
                "every JPEG decodes on its own; frame rate unknown, 10 fps assumed (use --fps)"
                    .into()
            } else {
                "every JPEG decodes on its own".into()
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
    fn minimal_jpeg_structure() {
        // SOI, DQT, SOF0 (16x8), SOS, 2 bytes data with stuffed FF00, EOI.
        let mut b = vec![0xFF, 0xD8];
        b.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x43, 0x00]);
        b.extend_from_slice(&[1u8; 64]);
        b.extend_from_slice(&[
            0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x08, 0x00, 0x10, 0x01, 0x01, 0x11, 0x00,
        ]);
        b.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00]);
        b.extend_from_slice(&[0x12, 0xFF, 0x00, 0x34, 0xFF, 0xD9, 0xAA]);
        assert_eq!(parse(&b), Some((b.len() - 1, 16, 8)));
        assert_eq!(parse(&b[..b.len() - 3]), None);
    }
}
