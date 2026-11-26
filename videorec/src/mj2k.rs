//! Carving of Motion JPEG 2000 frames (ISO/IEC 15444-1 codestreams, bare or
//! inside JP2 boxes) from raw bytes.
//!
//! A codestream starts with SOC + SIZ (`FF 4F FF 51`), whose SIZ segment
//! gives the picture size. The main header is a chain of marker segments
//! with lengths; each tile-part (SOT, `FF 90`) states its own length, and
//! the stream ends with EOC (`FF D9`). A frame is accepted only when that
//! whole chain is followed to EOC. When the codestream sits in a `jp2c` box
//! with the JP2 signature box shortly before it, the frame starts at the
//! signature. Runs of same-sized frames are written to AVI (fourcc `mjp2`)
//! with the frame bytes unchanged.

use crate::io::Source;
use crate::mjpeg::{self, Flavour, Jpeg};
use crate::report::Recovered;
use anyhow::Result;
use std::path::Path;

pub(crate) const MAX_FRAME: usize = 16 << 20;
const SIGNATURE: [u8; 12] = [
    0, 0, 0, 0x0C, 0x6A, 0x50, 0x20, 0x20, 0x0D, 0x0A, 0x87, 0x0A,
];

const MJ2K: Flavour = Flavour {
    fourcc: *b"mjp2",
    prefix: "mj2k",
    kind: "carved_mj2k",
    codec: "jpeg2000",
    what: "every JPEG 2000 frame decodes on its own",
};

fn be16(b: &[u8], o: usize) -> Option<usize> {
    Some(u16::from_be_bytes(b.get(o..o + 2)?.try_into().ok()?) as usize)
}
fn be32(b: &[u8], o: usize) -> Option<u64> {
    Some(u32::from_be_bytes(b.get(o..o + 4)?.try_into().ok()?) as u64)
}

/// Follows a codestream at the start of `b`. Returns (length, width, height).
pub fn parse(b: &[u8]) -> Option<(usize, u32, u32)> {
    if b.get(..4)? != [0xFF, 0x4F, 0xFF, 0x51] {
        return None;
    }
    let lsiz = be16(b, 4)?;
    let comps = be16(b, 40)?;
    if !(1..=16).contains(&comps) || lsiz != 38 + 3 * comps {
        return None;
    }
    let w = be32(b, 8)?.checked_sub(be32(b, 16)?)?;
    let h = be32(b, 12)?.checked_sub(be32(b, 20)?)?;
    if !(16..=16384).contains(&w) || !(16..=16384).contains(&h) {
        return None;
    }
    let mut p = 4 + lsiz;
    let mut tiles = 0;
    loop {
        if *b.get(p)? != 0xFF {
            return None;
        }
        match *b.get(p + 1)? {
            0xD9 if tiles > 0 => return Some((p + 2, w as u32, h as u32)),
            0x90 => {
                if be16(b, p + 2)? != 10 {
                    return None;
                }
                tiles += 1;
                let psot = be32(b, p + 6)? as usize;
                if psot >= 14 {
                    p += psot;
                } else if psot == 0 {
                    // Last tile-part: runs to EOC, which cannot occur in data.
                    let k = memchr::memmem::find(b.get(p + 12..)?, &[0xFF, 0xD9])?;
                    return Some((p + 12 + k + 2, w as u32, h as u32));
                } else {
                    return None;
                }
            }
            // Main-header marker segments: COD, COC, TLM, PLM, QCD, QCC, RGN,
            // POC, PPM, CRG, COM.
            0x52 | 0x53 | 0x55 | 0x57 | 0x5C | 0x5D | 0x5E | 0x5F | 0x60 | 0x63 | 0x64
                if tiles == 0 =>
            {
                p += 2 + be16(b, p + 2)?;
            }
            _ => return None,
        }
    }
}

/// Frames starting in `[start, stop)` of a region ending at `end`. Returns
/// where to resume.
pub fn scan_window(
    src: &Source,
    start: u64,
    stop: u64,
    end: u64,
    found: &mut Vec<Jpeg>,
) -> Result<u64> {
    const BLOCK: usize = 16 << 20;
    let finder = memchr::memmem::Finder::new(&[0xFF, 0x4F, 0xFF, 0x51]);
    let mut pos = start;
    let mut buf = vec![0u8; BLOCK + MAX_FRAME];
    while pos < stop {
        crate::progress::at(pos);
        if crate::progress::stopped() {
            break;
        }
        let want = (BLOCK + MAX_FRAME).min((end - pos) as usize);
        let n = src.read_at(pos, &mut buf[..want])?;
        if n < 8 {
            break;
        }
        let lim = if n < want { n } else { n.min(BLOCK) };
        let lim = lim.min((stop - pos) as usize);
        let mut i = 0;
        let mut next = pos + lim as u64;
        while let Some(k) = finder.find(&buf[i.min(lim)..(lim + 3).min(n)]) {
            let at = i + k;
            if let Some((len, w, h)) = parse(&buf[at..n.min(at + MAX_FRAME)]) {
                let abs = pos + at as u64;
                // Inside a JP2 file: keep its boxes with the frame.
                let back = (abs - start).min(1024) as usize;
                let pre = src.read_vec(abs - back as u64, back)?;
                let from = found.last().map_or(start, |f: &Jpeg| f.end.max(start));
                let head = match memchr::memmem::rfind(&pre, &SIGNATURE) {
                    Some(s) if pre.ends_with(b"jp2c") && abs - (back - s) as u64 >= from => {
                        abs - (back - s) as u64
                    }
                    _ => abs,
                };
                found.push(Jpeg {
                    start: head,
                    end: abs + len as u64,
                    width: w.min(65535) as u16,
                    height: h.min(65535) as u16,
                });
                if at + len >= lim {
                    next = abs + len as u64;
                    break;
                }
                i = at + len;
            } else {
                i = at + 1;
            }
        }
        pos = next.max(pos + 1);
    }
    Ok(pos.max(stop))
}

pub fn write_runs(
    src: &Source,
    frames: &[Jpeg],
    out: &Path,
    first_id: usize,
    max_gap: u64,
    min_frames: usize,
    fps: Option<f64>,
) -> Result<(Vec<Recovered>, usize)> {
    mjpeg::write_runs_as(src, frames, out, first_id, max_gap, min_frames, fps, &MJ2K)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_codestream() {
        assert_eq!(parse(&[0xFF, 0x4F, 0xFF, 0x51, 0, 0, 0, 0]), None);
        assert_eq!(
            parse(b"not a codestream at all......................"),
            None
        );
    }
}
