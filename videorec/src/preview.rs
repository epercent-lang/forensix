//! Thumbnails and short previews of recovered files for the GUI.
//!
//! Browsers cannot play AVI, nor 16-bit PCM inside MP4, so the GUI gets:
//! - a thumbnail: a one-frame MP4 holding the first key frame (or the first
//!   JPEG of a Motion-JPEG file), which the page decodes and draws;
//! - a preview: a video-only MP4 of the first minutes, plus a WAV file when
//!   the recording has 16-bit PCM sound.
//!
//! Frame and sound bytes are copied, never re-encoded. Files are written to a
//! cache folder in the system temp directory, never next to the evidence.

use crate::codec::Codec;
use crate::mp4::{self, Mp4Writer, VideoConfig};
use anyhow::{Context, Result, bail};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy)]
struct Sample {
    off: u64,
    len: u32,
    key: bool,
    /// Duration in milliseconds.
    dur: u32,
}

struct Clip {
    /// Annex B samples (AVI) need converting to length-prefixed NAL units.
    annexb: bool,
    mjpeg: bool,
    codec: Codec,
    config: Option<VideoConfig>,
    samples: Vec<Sample>,
    /// 16-bit mono/stereo PCM pieces: (offset, bytes).
    audio: Vec<(u64, u32)>,
    audio_rate: u32,
    audio_channels: u16,
}

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}

fn read_at(f: &mut File, off: u64, len: usize) -> Result<Vec<u8>> {
    f.seek(SeekFrom::Start(off))?;
    let mut v = vec![0u8; len];
    f.read_exact(&mut v)?;
    Ok(v)
}

/// NAL units of an Annex B buffer.
fn split_nals(p: &[u8]) -> Vec<&[u8]> {
    let mut starts = vec![];
    let mut i = 0;
    while i + 3 <= p.len() {
        if p[i] == 0 && p[i + 1] == 0 && p[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = vec![];
    for (k, &s) in starts.iter().enumerate() {
        let mut e = starts.get(k + 1).map_or(p.len(), |&n| n - 3);
        while e > s && p[e - 1] == 0 {
            e -= 1;
        }
        if e > s {
            out.push(&p[s..e]);
        }
    }
    out
}

fn nal_type(codec: Codec, n: &[u8]) -> u8 {
    match codec {
        Codec::H264 => n[0] & 0x1F,
        Codec::H265 => (n[0] >> 1) & 0x3F,
    }
}

fn is_key_nal(codec: Codec, n: &[u8]) -> bool {
    match codec {
        Codec::H264 => nal_type(codec, n) == 5,
        Codec::H265 => (16..=21).contains(&nal_type(codec, n)),
    }
}

// ---------------------------------------------------------------- AVI

fn read_avi(f: &mut File, max_ms: u64) -> Result<Clip> {
    let size = f.metadata()?.len();
    let mut clip = Clip {
        annexb: true,
        mjpeg: false,
        codec: Codec::H264,
        config: None,
        samples: vec![],
        audio: vec![],
        audio_rate: 0,
        audio_channels: 1,
    };
    let mut frame_ms = 33u32;
    let mut vid_stream: Option<u8> = None;
    let mut aud_stream: Option<u8> = None;
    let mut stream_no = 0u8;
    let mut last_type = *b"    ";
    let mut elapsed = 0u64;
    // Explicit stack of list ends so nested LISTs (hdrl, strl, movi, rec) are walked.
    let mut pos = 0u64;
    let mut ends: Vec<u64> = vec![size];
    let mut pcm_ok = false;
    while pos + 8 <= size {
        while let Some(&e) = ends.last() {
            if pos >= e && ends.len() > 1 {
                ends.pop();
            } else {
                break;
            }
        }
        let h = read_at(f, pos, 12.min((size - pos) as usize))?;
        let id: [u8; 4] = h[0..4].try_into().unwrap();
        let len = le32(&h, 4) as u64;
        if &id == b"RIFF" || &id == b"LIST" {
            if h.len() < 12 {
                break;
            }
            let kind = &h[8..12];
            if kind == b"hdrl"
                || kind == b"strl"
                || kind == b"movi"
                || kind == b"rec "
                || kind == b"AVI "
                || kind == b"AVIX"
            {
                ends.push(pos + 8 + len);
                pos += 12;
                continue;
            }
            pos += 8 + len + (len & 1);
            continue;
        }
        let body = pos + 8;
        match &id {
            b"avih" if len >= 4 => {
                let b = read_at(f, body, 4)?;
                let us = le32(&b, 0);
                if us > 0 {
                    frame_ms = (us / 1000).max(1);
                }
            }
            b"strh" if len >= 32 => {
                let b = read_at(f, body, 32)?;
                last_type = b[0..4].try_into().unwrap();
                if &last_type == b"vids" && vid_stream.is_none() {
                    vid_stream = Some(stream_no);
                    let (scale, rate) = (le32(&b, 20), le32(&b, 24));
                    if scale > 0 && rate > 0 {
                        frame_ms = ((1000u64 * scale as u64) / rate as u64).max(1) as u32;
                    }
                } else if &last_type == b"auds" && aud_stream.is_none() {
                    aud_stream = Some(stream_no);
                }
                stream_no += 1;
            }
            b"strf" => {
                if &last_type == b"vids" && len >= 20 {
                    let b = read_at(f, body, 20)?;
                    match &b[16..20] {
                        b"MJPG" | b"mjpg" => clip.mjpeg = true,
                        b"H264" | b"h264" | b"X264" | b"x264" | b"AVC1" | b"avc1" => {}
                        b"HEVC" | b"hevc" | b"H265" | b"h265" | b"HVC1" | b"hvc1" => {
                            clip.codec = Codec::H265
                        }
                        other => bail!(
                            "AVI video format {} is not supported for preview",
                            String::from_utf8_lossy(other)
                        ),
                    }
                } else if &last_type == b"auds" && len >= 16 {
                    let b = read_at(f, body, 16)?;
                    let (tag, ch, rate, bits) =
                        (le16(&b, 0), le16(&b, 2), le32(&b, 4), le16(&b, 14));
                    if tag == 1 && bits == 16 && (1..=2).contains(&ch) {
                        pcm_ok = true;
                        clip.audio_rate = rate;
                        clip.audio_channels = ch;
                    }
                }
            }
            _ => {
                let sid = (id[0].wrapping_sub(b'0')) * 10 + id[1].wrapping_sub(b'0');
                let is_v = (&id[2..4] == b"dc" || &id[2..4] == b"db") && Some(sid) == vid_stream;
                let is_a = &id[2..4] == b"wb" && Some(sid) == aud_stream;
                if is_v && len > 0 {
                    if elapsed >= max_ms {
                        break;
                    }
                    clip.samples.push(Sample {
                        off: body,
                        len: len as u32,
                        key: clip.mjpeg,
                        dur: frame_ms,
                    });
                    elapsed += frame_ms as u64;
                } else if is_a && pcm_ok && len > 0 {
                    clip.audio.push((body, len as u32));
                }
            }
        }
        pos = body + len + (len & 1);
    }
    if clip.samples.is_empty() {
        bail!("no video frames found in this AVI");
    }
    if !clip.mjpeg {
        // Key frames and parameter sets from the Annex B data.
        let codec = clip.codec;
        let (mut vps, mut sps, mut pps) = (None, None, None);
        for s in clip.samples.iter_mut() {
            let p = read_at(f, s.off, s.len as usize)?;
            for n in split_nals(&p) {
                if n.is_empty() {
                    continue;
                }
                s.key |= is_key_nal(codec, n);
                let t = nal_type(codec, n);
                match (codec, t) {
                    (Codec::H264, 7) | (Codec::H265, 33) if sps.is_none() => sps = Some(n.to_vec()),
                    (Codec::H264, 8) | (Codec::H265, 34) if pps.is_none() => pps = Some(n.to_vec()),
                    (Codec::H265, 32) if vps.is_none() => vps = Some(n.to_vec()),
                    _ => {}
                }
            }
        }
        if let (Some(s), Some(p)) = (sps, pps) {
            clip.config = Some(VideoConfig {
                codec,
                length_size: 4,
                vps: vps.into_iter().collect(),
                sps: vec![s],
                pps: vec![p],
                fps: Some(1000.0 / frame_ms as f64),
                origin: "in-band".into(),
            });
        }
    }
    Ok(clip)
}

// ---------------------------------------------------------------- MP4

/// Top-level boxes of the file: returns the moov payload.
fn find_moov(f: &mut File) -> Result<Vec<u8>> {
    let size = f.metadata()?.len();
    let mut pos = 0u64;
    while pos + 8 <= size {
        let h = read_at(f, pos, 16.min((size - pos) as usize))?;
        let mut len = be32(&h, 0) as u64;
        let mut hdr = 8;
        if len == 1 && h.len() >= 16 {
            len = u64::from_be_bytes(h[8..16].try_into().unwrap());
            hdr = 16;
        } else if len == 0 {
            len = size - pos;
        }
        if len < hdr {
            break;
        }
        if &h[4..8] == b"moov" {
            anyhow::ensure!(len < 256 << 20, "moov too large");
            return read_at(f, pos + hdr, (len - hdr) as usize);
        }
        pos += len;
    }
    bail!("no moov box (not a finished MP4)")
}

struct Table {
    timescale: u32,
    sizes: Vec<u32>,
    offsets: Vec<u64>,
    durations: Vec<u32>,
    keys: Option<Vec<u32>>,
}

fn sample_table(mdia: &[u8]) -> Option<Table> {
    let timescale = mp4::child(mdia, b"mdhd").and_then(|m| match m.first() {
        Some(1) if m.len() >= 24 => Some(be32(m, 20)),
        Some(0) if m.len() >= 16 => Some(be32(m, 12)),
        _ => None,
    })?;
    let stbl = mp4::child(mdia, b"minf").and_then(|m| mp4::child(m, b"stbl"))?;
    let stsz = mp4::child(stbl, b"stsz")?;
    let fixed = be32(stsz, 4);
    let n = be32(stsz, 8) as usize;
    let sizes: Vec<u32> = if fixed > 0 {
        vec![fixed; n]
    } else {
        (0..n.min((stsz.len().saturating_sub(12)) / 4))
            .map(|k| be32(stsz, 12 + 4 * k))
            .collect()
    };
    let mut chunks = vec![];
    if let Some(stco) = mp4::child(stbl, b"stco") {
        for k in 0..(be32(stco, 4) as usize).min((stco.len() - 8) / 4) {
            chunks.push(be32(stco, 8 + 4 * k) as u64);
        }
    } else if let Some(co) = mp4::child(stbl, b"co64") {
        for k in 0..(be32(co, 4) as usize).min((co.len() - 8) / 8) {
            chunks.push(u64::from_be_bytes(
                co[8 + 8 * k..16 + 8 * k].try_into().ok()?,
            ));
        }
    }
    let stsc = mp4::child(stbl, b"stsc")?;
    let runs: Vec<(u32, u32)> = (0..(be32(stsc, 4) as usize).min((stsc.len() - 8) / 12))
        .map(|k| (be32(stsc, 8 + 12 * k), be32(stsc, 12 + 12 * k)))
        .collect();
    let mut offsets = Vec::with_capacity(sizes.len());
    let mut s = 0usize;
    for (ci, &co) in chunks.iter().enumerate() {
        let c1 = ci as u32 + 1;
        let per = runs.iter().rev().find(|r| r.0 <= c1).map_or(1, |r| r.1) as usize;
        let mut o = co;
        for _ in 0..per {
            if s >= sizes.len() {
                break;
            }
            offsets.push(o);
            o += sizes[s] as u64;
            s += 1;
        }
    }
    let mut durations = vec![];
    if let Some(stts) = mp4::child(stbl, b"stts") {
        for k in 0..(be32(stts, 4) as usize).min((stts.len() - 8) / 8) {
            let (c, d) = (be32(stts, 8 + 8 * k), be32(stts, 12 + 8 * k));
            durations.extend(std::iter::repeat_n(d, c as usize));
        }
    }
    durations.resize(offsets.len(), *durations.last().unwrap_or(&1));
    let keys = mp4::child(stbl, b"stss").map(|ss| {
        (0..(be32(ss, 4) as usize).min((ss.len() - 8) / 4))
            .map(|k| be32(ss, 8 + 4 * k))
            .collect()
    });
    Some(Table {
        timescale,
        sizes: sizes[..offsets.len()].to_vec(),
        offsets,
        durations,
        keys,
    })
}

fn read_mp4(f: &mut File, max_ms: u64) -> Result<Clip> {
    let moov = find_moov(f)?;
    let mut clip = Clip {
        annexb: false,
        mjpeg: false,
        codec: Codec::H264,
        config: None,
        samples: vec![],
        audio: vec![],
        audio_rate: 0,
        audio_channels: 1,
    };
    let video = mp4::parse_moov(&moov)
        .into_iter()
        .find_map(|v| v.config)
        .context("no H.264/H.265 video track")?;
    clip.codec = video.codec;
    for (t, trak) in mp4::children(&moov) {
        if t != b"trak" {
            continue;
        }
        let Some(mdia) = mp4::child(trak, b"mdia") else {
            continue;
        };
        let Some(hdlr) = mp4::child(mdia, b"hdlr") else {
            continue;
        };
        if hdlr.len() < 12 {
            continue;
        }
        let Some(tb) = sample_table(mdia) else {
            continue;
        };
        let ts = tb.timescale.max(1) as u64;
        match &hdlr[8..12] {
            b"vide" if clip.samples.is_empty() => {
                let mut elapsed = 0u64;
                for i in 0..tb.offsets.len() {
                    if elapsed >= max_ms {
                        break;
                    }
                    let d = ((tb.durations[i] as u64 * 1000) / ts).max(1) as u32;
                    clip.samples.push(Sample {
                        off: tb.offsets[i],
                        len: tb.sizes[i],
                        key: tb.keys.as_ref().is_none_or(|k| k.contains(&(i as u32 + 1))),
                        dur: d,
                    });
                    elapsed += d as u64;
                }
            }
            b"soun" => {
                // Only 16-bit little-endian PCM ('sowt') is passed on.
                let stsd = mp4::child(mdia, b"minf")
                    .and_then(|m| mp4::child(m, b"stbl"))
                    .and_then(|s| mp4::child(s, b"stsd"));
                let Some(stsd) = stsd else { continue };
                if stsd.len() < 8 + 36 || &stsd[12..16] != b"sowt" {
                    continue;
                }
                let e = &stsd[16..];
                clip.audio_channels = u16::from_be_bytes([e[16], e[17]]).max(1);
                clip.audio_rate = be32(e, 24) >> 16;
                let limit = max_ms * clip.audio_rate as u64 * 2 * clip.audio_channels as u64 / 1000;
                let mut got = 0u64;
                // Contiguous samples of one chunk: merge into pieces.
                let mut i = 0;
                while i < tb.offsets.len() && got < limit {
                    let start = tb.offsets[i];
                    let mut len = tb.sizes[i] as u64;
                    i += 1;
                    while i < tb.offsets.len() && tb.offsets[i] == start + len {
                        len += tb.sizes[i] as u64;
                        i += 1;
                    }
                    clip.audio.push((start, len as u32));
                    got += len;
                }
            }
            _ => {}
        }
    }
    clip.config = Some(video);
    if clip.samples.is_empty() {
        bail!("no video samples");
    }
    Ok(clip)
}

fn open_clip(path: &Path, max_ms: u64) -> Result<(File, Clip)> {
    let mut f = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut magic = [0u8; 12];
    f.read_exact(&mut magic)?;
    let clip = if &magic[0..4] == b"RIFF" {
        read_avi(&mut f, max_ms)?
    } else if &magic[4..8] == b"ftyp" || &magic[4..8] == b"mdat" || &magic[4..8] == b"moov" {
        read_mp4(&mut f, max_ms)?
    } else {
        bail!("not an MP4 or AVI file")
    };
    Ok((f, clip))
}

fn sample_bytes(f: &mut File, clip: &Clip, s: &Sample) -> Result<Vec<u8>> {
    let p = read_at(f, s.off, s.len as usize)?;
    if !clip.annexb {
        return Ok(p);
    }
    let mut out = Vec::with_capacity(p.len() + 16);
    for n in split_nals(&p) {
        out.extend_from_slice(&(n.len() as u32).to_be_bytes());
        out.extend_from_slice(n);
    }
    Ok(out)
}

pub fn cache_dir() -> PathBuf {
    std::env::temp_dir().join("videorec_preview")
}

/// Cache key from path, size and modification time.
fn key_of(path: &Path) -> Result<String> {
    use sha2::Digest;
    let m = std::fs::metadata(path)?;
    let mt = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis());
    let mut h = sha2::Sha256::new();
    h.update(path.to_string_lossy().as_bytes());
    h.update(m.len().to_le_bytes());
    h.update(mt.to_le_bytes());
    Ok(h.finalize()[..12]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

pub enum Thumb {
    Mp4(PathBuf),
    Jpeg(Vec<u8>),
}

/// A one-frame MP4 of the first key frame, or the first JPEG.
pub fn thumb(path: &Path) -> Result<Thumb> {
    let dir = cache_dir();
    std::fs::create_dir_all(&dir)?;
    let out = dir.join(format!("{}_t.mp4", key_of(path)?));
    if out.exists() {
        return Ok(Thumb::Mp4(out));
    }
    // Key frames can be far in; look through the first 10 minutes.
    let (mut f, clip) = open_clip(path, 600_000)?;
    if clip.mjpeg {
        let s = clip.samples[0];
        return Ok(Thumb::Jpeg(read_at(&mut f, s.off, s.len as usize)?));
    }
    let cfg = clip
        .config
        .clone()
        .context("no parameter sets (SPS/PPS) in this file")?;
    let s = *clip
        .samples
        .iter()
        .find(|s| s.key)
        .context("no key frame, so no picture can be shown")?;
    let entry = mp4::sample_entry(&cfg).context("cannot describe this video")?;
    let tmp = dir.join(format!("{}_t.part", key_of(path)?));
    let mut w = Mp4Writer::create(&tmp)?;
    w.add_sample(&sample_bytes(&mut f, &clip, &s)?, true)?;
    w.finish_timed(&entry, 1000, &[100])?;
    std::fs::rename(&tmp, &out)?;
    Ok(Thumb::Mp4(out))
}

pub struct Preview {
    pub video: PathBuf,
    pub audio: Option<PathBuf>,
    pub seconds: f64,
    pub truncated: bool,
    pub skipped_leading: usize,
}

/// Video-only MP4 of the first `max_s` seconds (from the first key frame)
/// and, with 16-bit PCM sound, a WAV of the same span.
pub fn preview(path: &Path, max_s: u64) -> Result<Preview> {
    let dir = cache_dir();
    std::fs::create_dir_all(&dir)?;
    let k = key_of(path)?;
    let (vout, aout) = (
        dir.join(format!("{k}_{max_s}_v.mp4")),
        dir.join(format!("{k}_{max_s}_a.wav")),
    );
    let (mut f, clip) = open_clip(path, max_s * 1000 + 1)?;
    anyhow::ensure!(
        !clip.mjpeg,
        "Motion-JPEG files cannot be previewed here; open them in the default player"
    );
    let cfg = clip
        .config
        .clone()
        .context("no parameter sets (SPS/PPS) in this file")?;
    let first = clip.samples.iter().position(|s| s.key).context(
        "no key frame, so the browser cannot show this video; open it in the default player",
    )?;
    let lead_ms: u64 = clip.samples[..first].iter().map(|s| s.dur as u64).sum();
    let total_ms: u64 = clip.samples.iter().map(|s| s.dur as u64).sum();
    let truncated = total_ms >= max_s * 1000;
    if !vout.exists() {
        let entry = mp4::sample_entry(&cfg).context("cannot describe this video")?;
        let tmp = dir.join(format!("{k}_{max_s}_v.part"));
        let mut w = Mp4Writer::create(&tmp)?;
        let mut durs = vec![];
        for s in &clip.samples[first..] {
            w.add_sample(&sample_bytes(&mut f, &clip, s)?, s.key)?;
            durs.push(s.dur);
        }
        w.finish_timed(&entry, 1000, &durs)?;
        std::fs::rename(&tmp, &vout)?;
    }
    let mut audio = None;
    if !clip.audio.is_empty() && clip.audio_rate > 0 {
        if !aout.exists() {
            let bps = clip.audio_rate as u64 * 2 * clip.audio_channels as u64;
            // Drop the sound that belongs to frames before the first key frame.
            let mut skip = (lead_ms * bps / 1000) & !1;
            let mut pcm = vec![];
            let want = (total_ms - lead_ms) * bps / 1000;
            for &(o, l) in &clip.audio {
                let mut b = read_at(&mut f, o, l as usize)?;
                if skip > 0 {
                    let d = (skip as usize).min(b.len());
                    b.drain(..d);
                    skip -= d as u64;
                }
                pcm.extend_from_slice(&b);
                if pcm.len() as u64 >= want {
                    pcm.truncate(want as usize & !1);
                    break;
                }
            }
            let tmp = dir.join(format!("{k}_{max_s}_a.part"));
            let mut w = File::create(&tmp)?;
            let ch = clip.audio_channels;
            let mut h = vec![];
            h.extend_from_slice(b"RIFF");
            h.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
            h.extend_from_slice(b"WAVEfmt ");
            h.extend_from_slice(&16u32.to_le_bytes());
            h.extend_from_slice(&1u16.to_le_bytes());
            h.extend_from_slice(&ch.to_le_bytes());
            h.extend_from_slice(&clip.audio_rate.to_le_bytes());
            h.extend_from_slice(&(bps as u32).to_le_bytes());
            h.extend_from_slice(&(2 * ch).to_le_bytes());
            h.extend_from_slice(&16u16.to_le_bytes());
            h.extend_from_slice(b"data");
            h.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
            w.write_all(&h)?;
            w.write_all(&pcm)?;
            drop(w);
            std::fs::rename(&tmp, &aout)?;
        }
        audio = Some(aout);
    }
    Ok(Preview {
        video: vout,
        audio,
        seconds: (total_ms - lead_ms) as f64 / 1000.0,
        truncated,
        skipped_leading: first,
    })
}
