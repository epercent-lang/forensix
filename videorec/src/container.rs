//! Pass 1: container carving (MP4/MOV/3GP and AVI) and collection of decoder
//! configurations (avcC/hvcC) found anywhere on the medium.
//!
//! References: ISO/IEC 14496-12 (MP4 boxes); Microsoft "AVI RIFF File Reference"
//! and OpenDML AVI extensions (RIFF 'AVIX' continuation).

use crate::codec;
use crate::io::{Source, be32, le32};
use crate::mp4::{self, VideoConfig};
use anyhow::Result;

/// A container file whose structure was followed from start to end.
#[derive(Debug, Clone)]
pub struct Container {
    pub kind: &'static str, // "mp4" | "avi"
    pub start: u64,
    pub end: u64,
    /// True when every structural check passed (all video chunk offsets of an
    /// MP4 point at valid NAL units; AVI chunk chain is unbroken).
    pub intact: bool,
    pub note: String,
    pub config: Option<VideoConfig>,
    /// From the MP4 movie header (mvhd), when present.
    pub duration_s: Option<f64>,
}

/// Checks a candidate MP4 at `start` (limited to `limit`).
pub fn check_mp4(src: &Source, start: u64, limit: u64) -> Result<Option<Container>> {
    let w = mp4::walk(src, start, limit)?;
    if w.boxes.is_empty() || !w.has_mdat {
        return Ok(None);
    }
    let mut c = Container {
        kind: "mp4",
        start,
        end: w.end,
        intact: false,
        note: String::new(),
        config: None,
        duration_s: None,
    };
    if w.open_ended {
        c.note = "box chain runs past the readable area or mdat has size 0 (incomplete recording)"
            .into();
        return Ok(Some(c));
    }
    if !w.has_moov {
        c.note = "no moov box (recording not finalised or moov overwritten)".into();
        return Ok(Some(c));
    }
    let moov = w.boxes.iter().find(|b| &b.typ == b"moov").unwrap();
    if moov.size > 256 << 20 {
        c.note = "moov too large".into();
        return Ok(Some(c));
    }
    let data = src.read_vec(moov.offset + 8, (moov.size - 8) as usize)?;
    c.duration_s = mvhd_duration(&data);
    let tracks = mp4::parse_moov(&data);
    let mut checked = 0usize;
    let mut bad = 0usize;
    for t in &tracks {
        let Some(cfg) = &t.config else { continue };
        if c.config.is_none() {
            let mut cfg = cfg.clone();
            cfg.origin = format!("moov@{:#x}", moov.offset);
            c.config = Some(cfg);
        }
        let ls = cfg.length_size as usize;
        for &co in &t.chunk_offsets {
            checked += 1;
            let abs = start + co;
            if abs + ls as u64 + 2 > c.end {
                bad += 1;
                continue;
            }
            let b = src.read_vec(abs, ls + 3)?;
            let len = b[..ls].iter().fold(0u64, |a, &x| (a << 8) | x as u64);
            if len < 2
                || abs + ls as u64 + len > c.end
                || codec::classify(cfg.codec, &b[ls..]).is_none()
            {
                bad += 1;
            }
        }
    }
    if checked == 0 {
        c.note = "moov has no H.264/H.265 video track to validate".into();
        // Keep non-validated files out of the "covered" set.
        return Ok(Some(c));
    }
    c.intact = bad == 0;
    if bad > 0 {
        c.note = format!(
            "{bad} of {checked} video chunks do not start with a valid NAL unit (overwritten or fragmented)"
        );
    }
    Ok(Some(c))
}

/// Duration in seconds from the `mvhd` box at the start of a moov payload
/// (ISO/IEC 14496-12, 8.2.2: version 0 has 32-bit, version 1 64-bit fields).
fn mvhd_duration(moov: &[u8]) -> Option<f64> {
    let mut p = 0usize;
    while p + 8 <= moov.len() {
        let size = u32::from_be_bytes(moov[p..p + 4].try_into().ok()?) as usize;
        if size < 8 || p + size > moov.len() {
            return None;
        }
        if &moov[p + 4..p + 8] == b"mvhd" {
            let b = &moov[p + 8..p + size];
            let (ts, dur) = if b.first() == Some(&1) && b.len() >= 32 {
                (
                    u32::from_be_bytes(b[20..24].try_into().ok()?) as u64,
                    u64::from_be_bytes(b[24..32].try_into().ok()?),
                )
            } else if b.len() >= 20 {
                (
                    u32::from_be_bytes(b[12..16].try_into().ok()?) as u64,
                    u32::from_be_bytes(b[16..20].try_into().ok()?) as u64,
                )
            } else {
                return None;
            };
            return (ts > 0).then(|| dur as f64 / ts as f64);
        }
        p += size;
    }
    None
}

fn is_chunk_id(id: &[u8]) -> bool {
    id.iter()
        .all(|&c| c.is_ascii_alphanumeric() || c == b' ' || c == b'_')
}

/// Walks a RIFF AVI or AVIX list starting at `start`. Returns the end offset and
/// whether every chunk inside the 'movi' list was well formed.
fn walk_riff(src: &Source, start: u64, limit: u64) -> Result<Option<(u64, bool)>> {
    let h = src.read_vec(start, 12)?;
    if h.len() < 12 || &h[0..4] != b"RIFF" || !(&h[8..12] == b"AVI " || &h[8..12] == b"AVIX") {
        return Ok(None);
    }
    let size = le32(&h, 4) as u64;
    let end = start + 8 + size + (size & 1);
    if size < 4 || end > limit {
        return Ok(Some((limit.min(end), false)));
    }
    let mut ok = true;
    let mut p = start + 12;
    let mut hdr = [0u8; 12];
    while p + 8 <= end {
        src.read_exact_at(p, &mut hdr[..12.min((end - p) as usize)])?;
        let id = &hdr[0..4];
        let sz = le32(&hdr, 4) as u64;
        if !is_chunk_id(id) || p + 8 + sz > end {
            ok = false;
            break;
        }
        if id == b"LIST" && &hdr[8..12] == b"movi" {
            // Walk every chunk in movi.
            let mend = p + 8 + sz;
            let mut q = p + 12;
            let mut ch = [0u8; 8];
            while q + 8 <= mend {
                src.read_exact_at(q, &mut ch)?;
                let csz = le32(&ch, 4) as u64;
                if !is_chunk_id(&ch[0..4]) || q + 8 + csz > mend {
                    ok = false;
                    break;
                }
                q += 8 + csz + (csz & 1);
            }
        }
        p += 8 + sz + (sz & 1);
    }
    Ok(Some((end, ok)))
}

pub fn check_avi(src: &Source, start: u64, limit: u64) -> Result<Option<Container>> {
    let Some((mut end, mut ok)) = walk_riff(src, start, limit)? else {
        return Ok(None);
    };
    let h = src.read_vec(start + 12, 12)?;
    if h.len() < 12 || &h[0..4] != b"LIST" || &h[8..12] != b"hdrl" {
        return Ok(None);
    }
    // OpenDML: further RIFF 'AVIX' lists follow directly.
    while ok && end + 12 <= limit {
        match walk_riff(src, end, limit)? {
            Some((e, o)) if src.read_vec(end + 8, 4)? == b"AVIX" => {
                end = e;
                ok = o;
            }
            _ => break,
        }
    }
    Ok(Some(Container {
        kind: "avi",
        start,
        end,
        intact: ok,
        note: if ok {
            String::new()
        } else {
            "RIFF chunk chain broken (overwritten or fragmented)".into()
        },
        config: None,
        duration_s: None,
    }))
}

/// Scans `[start, end)` at 512-byte alignment for container starts and
/// anywhere for avcC/hvcC boxes. Calls `found` for each container.
pub fn scan(
    src: &Source,
    start: u64,
    end: u64,
    configs: &mut Vec<VideoConfig>,
    found: &mut dyn FnMut(Container) -> Result<u64>,
) -> Result<()> {
    const CHUNK: usize = 8 << 20;
    let mut pos = start - start % 512;
    if pos < start {
        pos += 512;
    }
    let finders = [
        memchr::memmem::Finder::new(b"avcC"),
        memchr::memmem::Finder::new(b"hvcC"),
    ];
    while pos < end {
        let len = CHUNK.min((end - pos) as usize);
        let buf = src.read_vec(pos, len + 4096)?;
        let real = len.min(buf.len());
        // Decoder configs.
        for (fi, f) in finders.iter().enumerate() {
            for hit in f.find_iter(&buf[..real]) {
                if hit < 4 {
                    continue;
                }
                let size = be32(&buf, hit - 4) as usize;
                if !(16..=4096).contains(&size) || hit - 4 + size > buf.len() {
                    continue;
                }
                let payload = &buf[hit + 4..hit - 4 + size];
                let cfg = if fi == 0 {
                    mp4::parse_avcc(payload)
                } else {
                    mp4::parse_hvcc(payload)
                };
                if let Some(mut c) = cfg
                    && !configs.iter().any(|x| x.key() == c.key())
                {
                    c.origin = format!(
                        "{}@{:#x}",
                        if fi == 0 { "avcC" } else { "hvcC" },
                        pos + hit as u64 - 4
                    );
                    configs.push(c);
                }
            }
        }
        // Container starts at sector boundaries.
        let mut i = 0usize;
        let mut next = pos + len as u64;
        while i + 16 <= real {
            let s = &buf[i..];
            let abs = pos + i as u64;
            let hit = if mp4::looks_like_ftyp(s) {
                check_mp4(src, abs, end)?
            } else if &s[0..4] == b"RIFF" && &s[8..12] == b"AVI " {
                check_avi(src, abs, end)?
            } else {
                None
            };
            if let Some(c) = hit {
                if let Some(cfg) = &c.config {
                    if !configs.iter().any(|x| x.key() == cfg.key()) {
                        configs.push(cfg.clone());
                    } else if let Some(x) = configs.iter_mut().find(|x| x.key() == cfg.key())
                        && x.fps.is_none()
                    {
                        x.fps = cfg.fps;
                    }
                }
                let resume = found(c)?;
                if resume > abs + 512 {
                    // Skip past an intact container.
                    let r = resume.div_ceil(512) * 512;
                    if r >= pos + len as u64 {
                        next = r;
                        break;
                    }
                    i = (r - pos) as usize;
                    continue;
                }
            }
            i += 512;
        }
        pos = next;
    }
    Ok(())
}
