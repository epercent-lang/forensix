//! ISO Base Media File Format (MP4/MOV/3GP) parsing and writing.
//!
//! Reference: ISO/IEC 14496-12 (box structure, sample tables) and
//! ISO/IEC 14496-15 (avcC / hvcC decoder configuration records).

use crate::codec::{self, Codec, SpsInfo};
use crate::io::{Source, be32, be64};
use anyhow::Result;
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

/// Top-level box types accepted when walking a carved file.
const TOP_LEVEL: &[&[u8; 4]] = &[
    b"ftyp", b"moov", b"mdat", b"free", b"skip", b"wide", b"uuid", b"meta", b"moof", b"mfra",
    b"styp", b"sidx", b"pnot", b"udta", b"junk", b"PICT",
];

#[derive(Debug, Clone)]
pub struct BoxHit {
    pub typ: [u8; 4],
    pub offset: u64, // absolute in source
    pub size: u64,
}

/// Result of walking the top-level boxes from a candidate `ftyp`.
#[derive(Debug, Clone)]
pub struct Mp4Walk {
    /// End of the last box that could be followed (absolute).
    pub end: u64,
    pub boxes: Vec<BoxHit>,
    pub has_moov: bool,
    pub has_mdat: bool,
    /// The chain ended because a box claimed to run past the source end or
    /// had size 0 ("to end of file"), so the real end is unknown.
    pub open_ended: bool,
}

fn is_printable_4cc(t: &[u8]) -> bool {
    t.iter().all(|&c| c.is_ascii_alphanumeric() || c == b' ')
}

/// Checks whether `buf` (read at a sector boundary) looks like the start of an MP4.
pub fn looks_like_ftyp(buf: &[u8]) -> bool {
    if buf.len() < 16 || &buf[4..8] != b"ftyp" {
        return false;
    }
    let size = be32(buf, 0);
    (12..=1024).contains(&size) && is_printable_4cc(&buf[8..12])
}

pub fn walk(src: &Source, start: u64, limit: u64) -> Result<Mp4Walk> {
    let mut w = Mp4Walk {
        end: start,
        boxes: vec![],
        has_moov: false,
        has_mdat: false,
        open_ended: false,
    };
    let mut pos = start;
    let mut hdr = [0u8; 16];
    while pos + 8 <= limit {
        if src.read_at(pos, &mut hdr)? < 8 {
            break;
        }
        let typ: [u8; 4] = hdr[4..8].try_into().unwrap();
        if !TOP_LEVEL.contains(&&typ) {
            break;
        }
        if w.boxes.is_empty() != (&typ == b"ftyp") {
            // Must start with ftyp; a second ftyp is the next file.
            break;
        }
        let mut size = be32(&hdr, 0) as u64;
        if size == 1 {
            size = be64(&hdr, 8);
            if size < 16 {
                break;
            }
        } else if size == 0 {
            w.open_ended = true;
            if &typ == b"mdat" {
                w.has_mdat = true;
            }
            w.boxes.push(BoxHit {
                typ,
                offset: pos,
                size: 0,
            });
            break;
        } else if size < 8 {
            break;
        }
        if pos + size > limit {
            w.open_ended = true;
            if &typ == b"mdat" {
                w.has_mdat = true;
            }
            w.boxes.push(BoxHit {
                typ,
                offset: pos,
                size,
            });
            break;
        }
        w.has_moov |= &typ == b"moov";
        w.has_mdat |= &typ == b"mdat";
        w.boxes.push(BoxHit {
            typ,
            offset: pos,
            size,
        });
        pos += size;
        w.end = pos;
    }
    Ok(w)
}

/// Decoder configuration (parameter sets) for one video track.
#[derive(Debug, Clone)]
pub struct VideoConfig {
    pub codec: Codec,
    pub length_size: u8,
    pub vps: Vec<Vec<u8>>,
    pub sps: Vec<Vec<u8>>,
    pub pps: Vec<Vec<u8>>,
    pub fps: Option<f64>,
    /// Where the config was found (for the report).
    pub origin: String,
}

impl VideoConfig {
    pub fn sps_info(&self) -> Option<SpsInfo> {
        self.sps
            .first()
            .and_then(|s| codec::parse_sps(self.codec, s))
    }
    pub fn key(&self) -> (Codec, Vec<Vec<u8>>, Vec<Vec<u8>>, Vec<Vec<u8>>) {
        (
            self.codec,
            self.vps.clone(),
            self.sps.clone(),
            self.pps.clone(),
        )
    }
}

/// Parses an avcC payload (after the 8-byte box header).
pub fn parse_avcc(p: &[u8]) -> Option<VideoConfig> {
    if p.len() < 7 || p[0] != 1 {
        return None;
    }
    let length_size = (p[4] & 3) + 1;
    let mut i = 5;
    let nsps = (p[i] & 0x1F) as usize;
    i += 1;
    let mut sps = vec![];
    for _ in 0..nsps {
        let l = u16::from_be_bytes([*p.get(i)?, *p.get(i + 1)?]) as usize;
        sps.push(p.get(i + 2..i + 2 + l)?.to_vec());
        i += 2 + l;
    }
    let npps = *p.get(i)? as usize;
    i += 1;
    let mut pps = vec![];
    for _ in 0..npps {
        let l = u16::from_be_bytes([*p.get(i)?, *p.get(i + 1)?]) as usize;
        pps.push(p.get(i + 2..i + 2 + l)?.to_vec());
        i += 2 + l;
    }
    if sps.is_empty() || pps.is_empty() || codec::parse_sps(Codec::H264, &sps[0]).is_none() {
        return None;
    }
    Some(VideoConfig {
        codec: Codec::H264,
        length_size,
        vps: vec![],
        sps,
        pps,
        fps: None,
        origin: String::new(),
    })
}

/// Parses an hvcC payload (after the 8-byte box header).
pub fn parse_hvcc(p: &[u8]) -> Option<VideoConfig> {
    if p.len() < 23 || p[0] != 1 {
        return None;
    }
    let length_size = (p[21] & 3) + 1;
    let narrays = p[22] as usize;
    let mut i = 23;
    let (mut vps, mut sps, mut pps) = (vec![], vec![], vec![]);
    for _ in 0..narrays {
        let t = *p.get(i)? & 0x3F;
        let n = u16::from_be_bytes([*p.get(i + 1)?, *p.get(i + 2)?]) as usize;
        i += 3;
        for _ in 0..n {
            let l = u16::from_be_bytes([*p.get(i)?, *p.get(i + 1)?]) as usize;
            let nal = p.get(i + 2..i + 2 + l)?.to_vec();
            i += 2 + l;
            match t {
                32 => vps.push(nal),
                33 => sps.push(nal),
                34 => pps.push(nal),
                _ => {}
            }
        }
    }
    if sps.is_empty() || pps.is_empty() || codec::parse_sps(Codec::H265, &sps[0]).is_none() {
        return None;
    }
    Some(VideoConfig {
        codec: Codec::H265,
        length_size,
        vps,
        sps,
        pps,
        fps: None,
        origin: String::new(),
    })
}

/// Iterates child boxes of a container payload.
pub(crate) fn children(p: &[u8]) -> Vec<(&[u8], &[u8])> {
    let mut out = vec![];
    let mut i = 0;
    while i + 8 <= p.len() {
        let mut size = be32(p, i) as usize;
        let mut hdr = 8;
        if size == 1 && i + 16 <= p.len() {
            size = be64(p, i + 8) as usize;
            hdr = 16;
        } else if size == 0 {
            size = p.len() - i;
        }
        if size < hdr || i + size > p.len() {
            break;
        }
        out.push((&p[i + 4..i + 8], &p[i + hdr..i + size]));
        i += size;
    }
    out
}

pub(crate) fn child<'a>(p: &'a [u8], t: &[u8; 4]) -> Option<&'a [u8]> {
    children(p)
        .into_iter()
        .find(|(ty, _)| ty == t)
        .map(|(_, b)| b)
}

/// Information about the video track of a parsed `moov`.
#[derive(Debug, Clone)]
pub struct MoovVideo {
    pub config: Option<VideoConfig>,
    /// Chunk offsets (as stored, relative to file start).
    pub chunk_offsets: Vec<u64>,
}

/// Parses a `moov` payload and returns its video tracks.
pub fn parse_moov(moov: &[u8]) -> Vec<MoovVideo> {
    let mut out = vec![];
    for (t, trak) in children(moov) {
        if t != b"trak" {
            continue;
        }
        let Some(mdia) = child(trak, b"mdia") else {
            continue;
        };
        let Some(hdlr) = child(mdia, b"hdlr") else {
            continue;
        };
        if hdlr.len() < 12 || &hdlr[8..12] != b"vide" {
            continue;
        }
        let timescale = child(mdia, b"mdhd").and_then(|m| match m.first() {
            Some(1) if m.len() >= 24 => Some(be32(m, 20)),
            Some(0) if m.len() >= 16 => Some(be32(m, 12)),
            _ => None,
        });
        let Some(stbl) = child(mdia, b"minf").and_then(|m| child(m, b"stbl")) else {
            continue;
        };
        let mut config = None;
        if let Some(stsd) = child(stbl, b"stsd")
            && stsd.len() > 8
        {
            for (et, entry) in children(&stsd[8..]) {
                // VisualSampleEntry header is 78 bytes before child boxes.
                if entry.len() < 78 {
                    continue;
                }
                let codec_box = match et {
                    b"avc1" | b"avc3" => child(&entry[78..], b"avcC").and_then(parse_avcc),
                    b"hvc1" | b"hev1" => child(&entry[78..], b"hvcC").and_then(parse_hvcc),
                    _ => None,
                };
                if codec_box.is_some() {
                    config = codec_box;
                    break;
                }
            }
        }
        if let (Some(c), Some(ts), Some(stts)) = (config.as_mut(), timescale, child(stbl, b"stts"))
            && stts.len() >= 16
            && be32(stts, 4) >= 1
        {
            let delta = be32(stts, 12);
            if delta > 0 && ts > 0 {
                c.fps = Some(ts as f64 / delta as f64);
            }
        }
        let mut chunk_offsets = vec![];
        if let Some(stco) = child(stbl, b"stco") {
            if stco.len() >= 8 {
                let n = be32(stco, 4) as usize;
                for k in 0..n.min((stco.len() - 8) / 4) {
                    chunk_offsets.push(be32(stco, 8 + k * 4) as u64);
                }
            }
        } else if let Some(co64) = child(stbl, b"co64")
            && co64.len() >= 8
        {
            let n = be32(co64, 4) as usize;
            for k in 0..n.min((co64.len() - 8) / 8) {
                chunk_offsets.push(be64(co64, 8 + k * 8));
            }
        }
        out.push(MoovVideo {
            config,
            chunk_offsets,
        });
    }
    out
}

/// Reads a parsed reference file (a healthy recording from the same device).
pub fn read_reference(path: &Path) -> Result<Option<VideoConfig>> {
    let src = Source::open(path)?;
    let w = walk(&src, 0, src.len())?;
    for b in &w.boxes {
        if &b.typ == b"moov" && b.size > 8 && b.size < 256 << 20 {
            let data = src.read_vec(b.offset + 8, (b.size - 8) as usize)?;
            for v in parse_moov(&data) {
                if let Some(mut c) = v.config {
                    c.origin = format!("reference:{}", path.display());
                    return Ok(Some(c));
                }
            }
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------- writing

fn bx(typ: &[u8; 4], content: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(content.len() + 8);
    v.extend_from_slice(&((content.len() + 8) as u32).to_be_bytes());
    v.extend_from_slice(typ);
    v.extend_from_slice(content);
    v
}

fn full(typ: &[u8; 4], version: u8, flags: u32, content: &[u8]) -> Vec<u8> {
    let mut c = Vec::with_capacity(content.len() + 4);
    c.push(version);
    c.extend_from_slice(&flags.to_be_bytes()[1..]);
    c.extend_from_slice(content);
    bx(typ, &c)
}

const MATRIX: [u32; 9] = [0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x4000_0000];

fn matrix() -> Vec<u8> {
    MATRIX.iter().flat_map(|v| v.to_be_bytes()).collect()
}

pub fn build_avcc(sps: &[Vec<u8>], pps: &[Vec<u8>]) -> Vec<u8> {
    let s0 = &sps[0];
    let mut c = vec![1, s0[1], s0[2], s0[3], 0xFF, 0xE0 | sps.len() as u8];
    for s in sps {
        c.extend_from_slice(&(s.len() as u16).to_be_bytes());
        c.extend_from_slice(s);
    }
    c.push(pps.len() as u8);
    for p in pps {
        c.extend_from_slice(&(p.len() as u16).to_be_bytes());
        c.extend_from_slice(p);
    }
    if matches!(s0[1], 100 | 110 | 122 | 144)
        && let Some(i) = codec::parse_sps(Codec::H264, s0)
    {
        c.push(0xFC | i.chroma_format_idc as u8);
        c.push(0xF8 | (i.bit_depth_luma - 8) as u8);
        c.push(0xF8 | (i.bit_depth_chroma - 8) as u8);
        c.push(0);
    }
    bx(b"avcC", &c)
}

pub fn build_hvcc(vps: &[Vec<u8>], sps: &[Vec<u8>], pps: &[Vec<u8>], info: &SpsInfo) -> Vec<u8> {
    let mut c = vec![1];
    c.extend_from_slice(&info.hevc_ptl);
    c.extend_from_slice(&[0xF0, 0x00, 0xFC]);
    c.push(0xFC | info.chroma_format_idc as u8);
    c.push(0xF8 | (info.bit_depth_luma - 8) as u8);
    c.push(0xF8 | (info.bit_depth_chroma - 8) as u8);
    c.extend_from_slice(&[0, 0]); // avgFrameRate
    c.push(0x0F); // numTemporalLayers=1, temporalIdNested=1, lengthSizeMinusOne=3
    let arrays: Vec<(u8, &[Vec<u8>])> = [(32u8, vps), (33, sps), (34, pps)]
        .into_iter()
        .filter(|(_, a)| !a.is_empty())
        .collect();
    c.push(arrays.len() as u8);
    for (t, a) in arrays {
        c.push(0x80 | t);
        c.extend_from_slice(&(a.len() as u16).to_be_bytes());
        for n in a {
            c.extend_from_slice(&(n.len() as u16).to_be_bytes());
            c.extend_from_slice(n);
        }
    }
    bx(b"hvcC", &c)
}

/// Streams samples into a new MP4 file. Sample bytes are written exactly as
/// given (length-prefixed NAL units); nothing is re-encoded.
pub struct Mp4Writer {
    out: BufWriter<File>,
    pos: u64,
    mdat_start: u64,
    sizes: Vec<u32>,
    offsets: Vec<u64>,
    keys: Vec<u32>,
    /// PCM blocks written to mdat: (offset, bytes).
    audio: Vec<(u64, u32)>,
    audio_rate: u32,
}

impl Mp4Writer {
    pub fn create(path: &Path) -> Result<Self> {
        let mut out = BufWriter::new(File::create(path)?);
        let ftyp = bx(b"ftyp", b"isom\x00\x00\x02\x00isomiso2avc1mp41");
        out.write_all(&ftyp)?;
        let mdat_start = ftyp.len() as u64;
        // 64-bit mdat header; size patched in finish().
        out.write_all(&1u32.to_be_bytes())?;
        out.write_all(b"mdat")?;
        out.write_all(&0u64.to_be_bytes())?;
        Ok(Self {
            out,
            pos: mdat_start + 16,
            mdat_start,
            sizes: vec![],
            offsets: vec![],
            keys: vec![],
            audio: vec![],
            audio_rate: 0,
        })
    }

    pub fn add_sample(&mut self, data: &[u8], key: bool) -> Result<()> {
        self.out.write_all(data)?;
        self.offsets.push(self.pos);
        self.sizes.push(data.len() as u32);
        if key {
            self.keys.push(self.sizes.len() as u32);
        }
        self.pos += data.len() as u64;
        Ok(())
    }

    /// Adds a block of mono signed 16-bit little-endian PCM at `rate` Hz,
    /// stored as a second track (`sowt` sample entry). Bytes are written as
    /// given.
    pub fn add_audio(&mut self, data: &[u8], rate: u32) -> Result<()> {
        if data.len() < 2 {
            return Ok(());
        }
        let data = &data[..data.len() & !1];
        self.out.write_all(data)?;
        self.audio.push((self.pos, data.len() as u32));
        self.audio_rate = rate;
        self.pos += data.len() as u64;
        Ok(())
    }

    fn sound_trak(&self) -> Vec<u8> {
        let rate = self.audio_rate.max(1);
        let n: u64 = self.audio.iter().map(|a| a.1 as u64 / 2).sum();
        let dur_ms = (n * 1000 / rate as u64).min(u32::MAX as u64) as u32;
        let mut tkhd = vec![];
        tkhd.extend_from_slice(&[0u8; 8]);
        tkhd.extend_from_slice(&2u32.to_be_bytes());
        tkhd.extend_from_slice(&[0u8; 4]);
        tkhd.extend_from_slice(&dur_ms.to_be_bytes());
        tkhd.extend_from_slice(&[0u8; 8]);
        tkhd.extend_from_slice(&[0, 0, 0, 1]); // layer 0, alternate group 1
        tkhd.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
        tkhd.extend_from_slice(&[0, 0]);
        tkhd.extend_from_slice(&matrix());
        tkhd.extend_from_slice(&[0u8; 8]);

        let mut mdhd = vec![];
        mdhd.extend_from_slice(&[0u8; 8]);
        mdhd.extend_from_slice(&rate.to_be_bytes());
        mdhd.extend_from_slice(&(n.min(u32::MAX as u64) as u32).to_be_bytes());
        mdhd.extend_from_slice(&0x55C4u16.to_be_bytes());
        mdhd.extend_from_slice(&[0, 0]);

        let mut hdlr = vec![0u8; 4];
        hdlr.extend_from_slice(b"soun");
        hdlr.extend_from_slice(&[0u8; 12]);
        hdlr.extend_from_slice(b"SoundHandler\0");

        // SoundSampleEntry: 16-bit little-endian PCM, mono.
        let mut e = vec![0u8; 6];
        e.extend_from_slice(&1u16.to_be_bytes()); // data reference index
        e.extend_from_slice(&[0u8; 8]);
        e.extend_from_slice(&1u16.to_be_bytes()); // channels
        e.extend_from_slice(&16u16.to_be_bytes()); // bits
        e.extend_from_slice(&[0u8; 4]);
        e.extend_from_slice(&(rate << 16).to_be_bytes());
        let mut stsd = 1u32.to_be_bytes().to_vec();
        stsd.extend(bx(b"sowt", &e));

        let mut stts = 1u32.to_be_bytes().to_vec();
        stts.extend_from_slice(&(n as u32).to_be_bytes());
        stts.extend_from_slice(&1u32.to_be_bytes());

        // One mp4 chunk per block; a new stsc entry when the block size changes.
        let mut entries: Vec<(u32, u32)> = vec![];
        for (i, a) in self.audio.iter().enumerate() {
            let per = a.1 / 2;
            if entries.last().is_none_or(|e| e.1 != per) {
                entries.push((i as u32 + 1, per));
            }
        }
        let mut stsc = (entries.len() as u32).to_be_bytes().to_vec();
        for (first, per) in entries {
            stsc.extend_from_slice(&first.to_be_bytes());
            stsc.extend_from_slice(&per.to_be_bytes());
            stsc.extend_from_slice(&1u32.to_be_bytes());
        }
        let mut stsz = 2u32.to_be_bytes().to_vec();
        stsz.extend_from_slice(&(n as u32).to_be_bytes());
        let mut co64 = (self.audio.len() as u32).to_be_bytes().to_vec();
        for a in &self.audio {
            co64.extend_from_slice(&a.0.to_be_bytes());
        }
        let mut stbl = full(b"stsd", 0, 0, &stsd);
        stbl.extend(full(b"stts", 0, 0, &stts));
        stbl.extend(full(b"stsc", 0, 0, &stsc));
        stbl.extend(full(b"stsz", 0, 0, &stsz));
        stbl.extend(full(b"co64", 0, 0, &co64));
        let dref = full(
            b"dref",
            0,
            0,
            &[&1u32.to_be_bytes()[..], &full(b"url ", 0, 1, &[])].concat(),
        );
        let mut minf = full(b"smhd", 0, 0, &[0u8; 4]);
        minf.extend(bx(b"dinf", &dref));
        minf.extend(bx(b"stbl", &stbl));
        let mut mdia = full(b"mdhd", 0, 0, &mdhd);
        mdia.extend(full(b"hdlr", 0, 0, &hdlr));
        mdia.extend(bx(b"minf", &minf));
        let mut trak = full(b"tkhd", 0, 3, &tkhd);
        trak.extend(bx(b"mdia", &mdia));
        bx(b"trak", &trak)
    }

    /// Finishes with a constant frame rate.
    pub fn finish(self, sample_entry: &[u8], fps: f64) -> Result<()> {
        let timescale: u32 = 90000;
        let delta: u32 = (timescale as f64 / fps).round().max(1.0) as u32;
        let n = self.sizes.len();
        self.finish_timed(sample_entry, timescale, &vec![delta; n])
    }

    /// Finishes with one duration per sample (in `timescale` units), e.g. from
    /// recorder timestamps.
    pub fn finish_timed(
        mut self,
        sample_entry: &[u8],
        timescale: u32,
        durations: &[u32],
    ) -> Result<()> {
        let n = self.sizes.len() as u32;
        assert_eq!(durations.len(), self.sizes.len());
        let dur_ts: u64 = durations.iter().map(|&d| d as u64).sum();
        let dur_ms = (dur_ts * 1000 / timescale as u64).min(u32::MAX as u64) as u32;

        let audio_ms = (self.audio.iter().map(|a| a.1 as u64 / 2).sum::<u64>() * 1000
            / self.audio_rate.max(1) as u64)
            .min(u32::MAX as u64) as u32;
        let mut mvhd = vec![];
        mvhd.extend_from_slice(&[0u8; 8]);
        mvhd.extend_from_slice(&1000u32.to_be_bytes());
        mvhd.extend_from_slice(&dur_ms.max(audio_ms).to_be_bytes());
        mvhd.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        mvhd.extend_from_slice(&0x0100u16.to_be_bytes());
        mvhd.extend_from_slice(&[0u8; 10]);
        mvhd.extend_from_slice(&matrix());
        mvhd.extend_from_slice(&[0u8; 24]);
        mvhd.extend_from_slice(&(if self.audio.is_empty() { 2u32 } else { 3 }).to_be_bytes());

        let (w, h) = sample_entry_dims(sample_entry);
        let mut tkhd = vec![];
        tkhd.extend_from_slice(&[0u8; 8]);
        tkhd.extend_from_slice(&1u32.to_be_bytes());
        tkhd.extend_from_slice(&[0u8; 4]);
        tkhd.extend_from_slice(&dur_ms.to_be_bytes());
        tkhd.extend_from_slice(&[0u8; 16]); // reserved(8) layer alt_group volume reserved
        tkhd.extend_from_slice(&matrix());
        tkhd.extend_from_slice(&((w as u32) << 16).to_be_bytes());
        tkhd.extend_from_slice(&((h as u32) << 16).to_be_bytes());

        let mut mdhd = vec![];
        mdhd.extend_from_slice(&[0u8; 8]);
        mdhd.extend_from_slice(&timescale.to_be_bytes());
        mdhd.extend_from_slice(&(dur_ts.min(u32::MAX as u64) as u32).to_be_bytes());
        mdhd.extend_from_slice(&0x55C4u16.to_be_bytes());
        mdhd.extend_from_slice(&[0, 0]);

        let mut hdlr = vec![0u8; 4];
        hdlr.extend_from_slice(b"vide");
        hdlr.extend_from_slice(&[0u8; 12]);
        hdlr.extend_from_slice(b"VideoHandler\0");

        let mut stsd = 1u32.to_be_bytes().to_vec();
        stsd.extend_from_slice(sample_entry);

        // Run-length encoded sample durations.
        let mut runs: Vec<(u32, u32)> = vec![];
        for &d in durations {
            match runs.last_mut() {
                Some(r) if r.1 == d => r.0 += 1,
                _ => runs.push((1, d)),
            }
        }
        let mut stts = (runs.len() as u32).to_be_bytes().to_vec();
        for (c, d) in runs {
            stts.extend_from_slice(&c.to_be_bytes());
            stts.extend_from_slice(&d.to_be_bytes());
        }

        let mut stsc = 1u32.to_be_bytes().to_vec();
        stsc.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1]);

        let mut stsz = 0u32.to_be_bytes().to_vec();
        stsz.extend_from_slice(&n.to_be_bytes());
        for s in &self.sizes {
            stsz.extend_from_slice(&s.to_be_bytes());
        }
        let mut co64 = n.to_be_bytes().to_vec();
        for o in &self.offsets {
            co64.extend_from_slice(&o.to_be_bytes());
        }

        let mut stbl = full(b"stsd", 0, 0, &stsd);
        stbl.extend(full(b"stts", 0, 0, &stts));
        if !self.keys.is_empty() && self.keys.len() < self.sizes.len() {
            let mut stss = (self.keys.len() as u32).to_be_bytes().to_vec();
            for k in &self.keys {
                stss.extend_from_slice(&k.to_be_bytes());
            }
            stbl.extend(full(b"stss", 0, 0, &stss));
        }
        stbl.extend(full(b"stsc", 0, 0, &stsc));
        stbl.extend(full(b"stsz", 0, 0, &stsz));
        stbl.extend(full(b"co64", 0, 0, &co64));

        let dref = full(
            b"dref",
            0,
            0,
            &[&1u32.to_be_bytes()[..], &full(b"url ", 0, 1, &[])].concat(),
        );
        let mut minf = full(b"vmhd", 0, 1, &[0u8; 8]);
        minf.extend(bx(b"dinf", &dref));
        minf.extend(bx(b"stbl", &stbl));

        let mut mdia = full(b"mdhd", 0, 0, &mdhd);
        mdia.extend(full(b"hdlr", 0, 0, &hdlr));
        mdia.extend(bx(b"minf", &minf));

        let mut trak = full(b"tkhd", 0, 3, &tkhd);
        trak.extend(bx(b"mdia", &mdia));

        let mut moov = full(b"mvhd", 0, 0, &mvhd);
        moov.extend(bx(b"trak", &trak));
        if !self.audio.is_empty() {
            moov.extend(self.sound_trak());
        }
        let moov = bx(b"moov", &moov);

        self.out.write_all(&moov)?;
        let mdat_size = self.pos - self.mdat_start;
        self.out.seek(SeekFrom::Start(self.mdat_start + 8))?;
        self.out.write_all(&mdat_size.to_be_bytes())?;
        self.out.flush()?;
        Ok(())
    }
}

fn sample_entry_dims(e: &[u8]) -> (u16, u16) {
    if e.len() >= 36 {
        (
            u16::from_be_bytes([e[32], e[33]]),
            u16::from_be_bytes([e[34], e[35]]),
        )
    } else {
        (0, 0)
    }
}

/// Builds an avc1/hvc1 VisualSampleEntry box.
pub fn sample_entry(cfg: &VideoConfig) -> Option<Vec<u8>> {
    let info = cfg.sps_info()?;
    let (typ, conf) = match cfg.codec {
        Codec::H264 => (b"avc1", build_avcc(&cfg.sps, &cfg.pps)),
        Codec::H265 => (b"hvc1", build_hvcc(&cfg.vps, &cfg.sps, &cfg.pps, &info)),
    };
    let mut c = vec![0u8; 6];
    c.extend_from_slice(&1u16.to_be_bytes());
    c.extend_from_slice(&[0u8; 16]);
    c.extend_from_slice(&(info.width as u16).to_be_bytes());
    c.extend_from_slice(&(info.height as u16).to_be_bytes());
    c.extend_from_slice(&0x0048_0000u32.to_be_bytes());
    c.extend_from_slice(&0x0048_0000u32.to_be_bytes());
    c.extend_from_slice(&[0u8; 4]);
    c.extend_from_slice(&1u16.to_be_bytes());
    c.extend_from_slice(&[0u8; 32]);
    c.extend_from_slice(&0x0018u16.to_be_bytes());
    c.extend_from_slice(&0xFFFFu16.to_be_bytes());
    c.extend(conf);
    Some(bx(typ, &c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avcc_roundtrip() {
        // High-profile SPS/PPS for 640x360 as written by x264 (from a test file).
        let sps = vec![
            0x67, 0x64, 0x00, 0x1E, 0xAC, 0xB4, 0x05, 0x01, 0x7F, 0xCB, 0x80, 0x88, 0x00, 0x00,
            0x03, 0x00, 0x08, 0x00, 0x00, 0x03, 0x01, 0xE0, 0x78, 0xB1, 0x75,
        ];
        let pps = vec![0x68, 0xEF, 0x3C, 0xB0];
        let b = build_avcc(&[sps.clone()], &[pps.clone()]);
        let c = parse_avcc(&b[8..]).expect("parse");
        assert_eq!(c.sps[0], sps);
        assert_eq!(c.pps[0], pps);
        let i = c.sps_info().unwrap();
        assert_eq!((i.width, i.height), (640, 360));
    }
}
