//! AVI writer for one video stream (H.264 Annex B or Motion JPEG), with OpenDML
//! (AVI 2.0) indexes so files can grow past 1 GB.
//!
//! Layout follows the Microsoft AVI RIFF reference and the OpenDML AVI File
//! Format Extensions 1.02: a first `RIFF AVI ` (headers, `movi`, `ix00`,
//! legacy `idx1`) followed by `RIFF AVIX` parts, each under about 1 GB, with a
//! super index (`indx`) in the stream header pointing at every `ix00`.

use anyhow::Result;
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

/// Room reserved in the super index (one entry per RIFF part).
const SUPER_ENTRIES: usize = 1024;
const RIFF_LIMIT: u64 = 1 << 30;

struct Part {
    /// File offset of the `movi` fourcc (data offsets in idx1 are relative to it).
    movi_fourcc: u64,
    /// (offset of chunk data, size, key frame)
    frames: Vec<(u64, u32, bool)>,
    /// Audio chunks: (offset of chunk data, size).
    audio: Vec<(u64, u32)>,
}

pub struct AviWriter {
    f: BufWriter<File>,
    pos: u64,
    width: u32,
    height: u32,
    rate: u32,
    scale: u32,
    fourcc: [u8; 4],
    parts: Vec<Part>,
    /// (offset of RIFF size field, offset of movi LIST size field)
    open: (u64, u64),
    /// Offsets of fields patched at the end.
    avih_frames: u64,
    strh_length: u64,
    indx_start: u64,
    dmlh_frames: u64,
    max_frame: u32,
    super_index: Vec<(u64, u32, u32)>,
    riff_limit: u64,
    /// Sample rate of the optional PCM audio stream (mono, 16-bit).
    audio_rate: Option<u32>,
    audio_strh_length: u64,
    audio_indx_start: u64,
    audio_super_index: Vec<(u64, u32, u32)>,
    max_audio: u32,
}

impl AviWriter {
    /// `rate / scale` is the frame rate; `fourcc` e.g. `H264` or `MJPG`.
    pub fn create(
        path: &Path,
        fourcc: [u8; 4],
        width: u32,
        height: u32,
        rate: u32,
        scale: u32,
    ) -> Result<Self> {
        // Test hook: smaller RIFF parts to exercise the OpenDML path.
        let limit = std::env::var("VIDEOREC_AVI_RIFF_LIMIT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(RIFF_LIMIT);
        Self::with_limit(path, fourcc, width, height, rate, scale, limit, None)
    }

    /// Like `create`, plus a second stream of mono signed 16-bit PCM at
    /// `audio_rate` Hz (WAVE_FORMAT_PCM).
    pub fn create_with_audio(
        path: &Path,
        fourcc: [u8; 4],
        width: u32,
        height: u32,
        rate: u32,
        scale: u32,
        audio_rate: u32,
    ) -> Result<Self> {
        let limit = std::env::var("VIDEOREC_AVI_RIFF_LIMIT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(RIFF_LIMIT);
        Self::with_limit(
            path,
            fourcc,
            width,
            height,
            rate,
            scale,
            limit,
            Some(audio_rate.max(1)),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn with_limit(
        path: &Path,
        fourcc: [u8; 4],
        width: u32,
        height: u32,
        rate: u32,
        scale: u32,
        riff_limit: u64,
        audio_rate: Option<u32>,
    ) -> Result<Self> {
        let mut w = AviWriter {
            f: BufWriter::new(File::create(path)?),
            pos: 0,
            width,
            height,
            rate: rate.max(1),
            scale: scale.max(1),
            fourcc,
            parts: vec![],
            open: (0, 0),
            avih_frames: 0,
            strh_length: 0,
            indx_start: 0,
            dmlh_frames: 0,
            max_frame: 0,
            super_index: vec![],
            riff_limit,
            audio_rate,
            audio_strh_length: 0,
            audio_indx_start: 0,
            audio_super_index: vec![],
            max_audio: 0,
        };
        w.header()?;
        Ok(w)
    }

    fn put(&mut self, b: &[u8]) -> Result<()> {
        self.f.write_all(b)?;
        self.pos += b.len() as u64;
        Ok(())
    }
    fn u32(&mut self, v: u32) -> Result<()> {
        self.put(&v.to_le_bytes())
    }
    fn u16(&mut self, v: u16) -> Result<()> {
        self.put(&v.to_le_bytes())
    }
    fn patch(&mut self, at: u64, v: u32) -> Result<()> {
        self.f.seek(SeekFrom::Start(at))?;
        self.f.write_all(&v.to_le_bytes())?;
        self.f.seek(SeekFrom::Start(self.pos))?;
        Ok(())
    }
    /// Writes `id` and a size placeholder; returns the offset of the size field.
    fn open_chunk(&mut self, id: &[u8; 4]) -> Result<u64> {
        self.put(id)?;
        let at = self.pos;
        self.u32(0)?;
        Ok(at)
    }
    fn close_chunk(&mut self, size_at: u64) -> Result<()> {
        if self.pos % 2 == 1 {
            self.put(&[0])?;
        }
        let size = self.pos - size_at - 4;
        self.patch(size_at, size as u32)
    }

    fn header(&mut self) -> Result<()> {
        let riff = self.open_chunk(b"RIFF")?;
        self.put(b"AVI ")?;
        let hdrl = self.open_chunk(b"LIST")?;
        self.put(b"hdrl")?;
        // avih (MainAVIHeader)
        self.put(b"avih")?;
        self.u32(56)?;
        self.u32((1_000_000u64 * self.scale as u64 / self.rate as u64) as u32)?;
        self.u32(0)?; // max bytes per sec
        self.u32(0)?; // padding granularity
        self.u32(0x10 | 0x100)?; // AVIF_HASINDEX | AVIF_ISINTERLEAVED
        self.avih_frames = self.pos;
        self.u32(0)?; // total frames (first RIFF)
        self.u32(0)?; // initial frames
        self.u32(1 + self.audio_rate.is_some() as u32)?; // streams
        self.u32(0)?; // suggested buffer size (patched)
        self.u32(self.width)?;
        self.u32(self.height)?;
        self.put(&[0u8; 16])?;
        // stream list
        let strl = self.open_chunk(b"LIST")?;
        self.put(b"strl")?;
        self.put(b"strh")?;
        self.u32(56)?;
        self.put(b"vids")?;
        let fcc = self.fourcc;
        self.put(&fcc)?;
        self.u32(0)?; // flags
        self.u16(0)?; // priority
        self.u16(0)?; // language
        self.u32(0)?; // initial frames
        self.u32(self.scale)?;
        self.u32(self.rate)?;
        self.u32(0)?; // start
        self.strh_length = self.pos;
        self.u32(0)?; // length (patched)
        self.u32(0)?; // suggested buffer size (patched)
        self.u32(u32::MAX)?; // quality
        self.u32(0)?; // sample size
        self.u16(0)?;
        self.u16(0)?;
        self.u16(self.width as u16)?;
        self.u16(self.height as u16)?;
        // strf (BITMAPINFOHEADER)
        self.put(b"strf")?;
        self.u32(40)?;
        self.u32(40)?;
        self.u32(self.width)?;
        self.u32(self.height)?;
        self.u16(1)?;
        self.u16(24)?;
        self.put(&fcc)?;
        self.u32(self.width * self.height * 3)?;
        self.put(&[0u8; 16])?;
        // indx (super index), filled in at the end
        self.put(b"indx")?;
        self.u32(24 + 16 * SUPER_ENTRIES as u32)?;
        self.indx_start = self.pos;
        self.put(&vec![0u8; 24 + 16 * SUPER_ENTRIES])?;
        self.close_chunk(strl)?;
        if let Some(ar) = self.audio_rate {
            let strl = self.open_chunk(b"LIST")?;
            self.put(b"strl")?;
            self.put(b"strh")?;
            self.u32(56)?;
            self.put(b"auds")?;
            self.u32(0)?; // handler
            self.u32(0)?; // flags
            self.u16(0)?;
            self.u16(0)?;
            self.u32(0)?; // initial frames
            self.u32(2)?; // scale = block align
            self.u32(ar * 2)?; // rate = bytes per second
            self.u32(0)?; // start
            self.audio_strh_length = self.pos;
            self.u32(0)?; // length in blocks (patched)
            self.u32(0)?; // suggested buffer size (patched)
            self.u32(u32::MAX)?;
            self.u32(2)?; // sample size
            self.put(&[0u8; 8])?;
            // strf (WAVEFORMATEX)
            self.put(b"strf")?;
            self.u32(18)?;
            self.u16(1)?; // WAVE_FORMAT_PCM
            self.u16(1)?; // channels
            self.u32(ar)?;
            self.u32(ar * 2)?;
            self.u16(2)?; // block align
            self.u16(16)?; // bits
            self.u16(0)?; // cbSize
            self.put(b"indx")?;
            self.u32(24 + 16 * SUPER_ENTRIES as u32)?;
            self.audio_indx_start = self.pos;
            self.put(&vec![0u8; 24 + 16 * SUPER_ENTRIES])?;
            self.close_chunk(strl)?;
        }
        // OpenDML header
        let odml = self.open_chunk(b"LIST")?;
        self.put(b"odml")?;
        self.put(b"dmlh")?;
        self.u32(248)?;
        self.dmlh_frames = self.pos;
        self.put(&[0u8; 248])?;
        self.close_chunk(odml)?;
        self.close_chunk(hdrl)?;
        let movi = self.open_chunk(b"LIST")?;
        let movi_fourcc = self.pos;
        self.put(b"movi")?;
        self.open = (riff, movi);
        self.parts.push(Part {
            movi_fourcc,
            frames: vec![],
            audio: vec![],
        });
        Ok(())
    }

    /// Starts a new `RIFF AVIX` part when the current one would grow too big.
    fn maybe_split(&mut self, len: usize) -> Result<()> {
        let riff_start = self.open.0 - 4;
        let last = self.parts.last().unwrap();
        if !(last.frames.is_empty() && last.audio.is_empty())
            && self.pos + len as u64 + 8 - riff_start > self.riff_limit
        {
            self.end_part()?;
            let riff = self.open_chunk(b"RIFF")?;
            self.put(b"AVIX")?;
            let movi = self.open_chunk(b"LIST")?;
            let movi_fourcc = self.pos;
            self.put(b"movi")?;
            self.open = (riff, movi);
            self.parts.push(Part {
                movi_fourcc,
                frames: vec![],
                audio: vec![],
            });
        }
        Ok(())
    }

    /// Adds a block of PCM audio (only with `create_with_audio`).
    pub fn add_audio(&mut self, data: &[u8]) -> Result<()> {
        anyhow::ensure!(self.audio_rate.is_some(), "AVI has no audio stream");
        let data = &data[..data.len() & !1];
        if data.is_empty() {
            return Ok(());
        }
        self.maybe_split(data.len())?;
        self.put(b"01wb")?;
        self.u32(data.len() as u32)?;
        let at = self.pos;
        self.put(data)?;
        self.max_audio = self.max_audio.max(data.len() as u32);
        self.parts
            .last_mut()
            .unwrap()
            .audio
            .push((at, data.len() as u32));
        Ok(())
    }

    /// Adds one frame (Annex B bytes).
    pub fn add_frame(&mut self, data: &[u8], key: bool) -> Result<()> {
        self.maybe_split(data.len())?;
        self.put(b"00dc")?;
        self.u32(data.len() as u32)?;
        let at = self.pos;
        self.put(data)?;
        if data.len() % 2 == 1 {
            self.put(&[0])?;
        }
        self.max_frame = self.max_frame.max(data.len() as u32);
        self.parts
            .last_mut()
            .unwrap()
            .frames
            .push((at, data.len() as u32, key));
        Ok(())
    }

    /// Writes the part's standard index, closes movi, adds idx1 for the
    /// first part, and closes the RIFF.
    fn end_part(&mut self) -> Result<()> {
        let k = self.parts.len() - 1;
        let base = self.parts[k].movi_fourcc;
        let frames = std::mem::take(&mut self.parts[k].frames);
        let audio = std::mem::take(&mut self.parts[k].audio);
        // ix00 (standard index) inside movi
        let ix_start = self.pos;
        self.put(b"ix00")?;
        self.u32(24 + 8 * frames.len() as u32)?;
        self.u16(2)?; // longs per entry
        self.put(&[0, 1])?; // sub type, AVI_INDEX_OF_CHUNKS
        self.u32(frames.len() as u32)?;
        self.put(b"00dc")?;
        self.put(&base.to_le_bytes())?;
        self.u32(0)?;
        for &(at, size, key) in &frames {
            self.u32((at - base) as u32)?;
            self.u32(if key { size } else { size | 0x8000_0000 })?;
        }
        self.super_index
            .push((ix_start, (self.pos - ix_start) as u32, frames.len() as u32));
        if self.audio_rate.is_some() {
            let ix_start = self.pos;
            self.put(b"ix01")?;
            self.u32(24 + 8 * audio.len() as u32)?;
            self.u16(2)?;
            self.put(&[0, 1])?;
            self.u32(audio.len() as u32)?;
            self.put(b"01wb")?;
            self.put(&base.to_le_bytes())?;
            self.u32(0)?;
            for &(at, size) in &audio {
                self.u32((at - base) as u32)?;
                self.u32(size)?;
            }
            let blocks: u32 = audio.iter().map(|a| a.1 / 2).sum();
            self.audio_super_index
                .push((ix_start, (self.pos - ix_start) as u32, blocks));
        }
        let (riff, movi) = self.open;
        self.close_chunk(movi)?;
        if k == 0 {
            // idx1 lists both streams in file order.
            let mut all: Vec<(u64, u32, &[u8; 4], u32)> = frames
                .iter()
                .map(|&(at, size, key)| (at, size, b"00dc", if key { 0x10 } else { 0 }))
                .chain(audio.iter().map(|&(at, size)| (at, size, b"01wb", 0x10)))
                .collect();
            all.sort_by_key(|e| e.0);
            self.put(b"idx1")?;
            self.u32(16 * all.len() as u32)?;
            for (at, size, id, flags) in all {
                self.put(id)?;
                self.u32(flags)?;
                self.u32((at - 8 - base) as u32)?;
                self.u32(size)?;
            }
            let n = frames.len() as u32;
            self.patch(self.avih_frames, n)?;
        }
        self.close_chunk(riff)?;
        self.parts[k].frames = frames;
        self.parts[k].audio = audio;
        Ok(())
    }

    pub fn finish(mut self) -> Result<()> {
        self.end_part()?;
        let total: u32 = self.parts.iter().map(|p| p.frames.len() as u32).sum();
        anyhow::ensure!(
            self.super_index.len() <= SUPER_ENTRIES,
            "AVI too large for the super index"
        );
        self.patch(self.strh_length, total)?;
        self.patch(self.strh_length + 4, self.max_frame + 8)?;
        self.patch(self.avih_frames + 12, self.max_frame + 8)?;
        self.patch(self.dmlh_frames, total)?;
        let mut indx = vec![];
        indx.extend_from_slice(&4u16.to_le_bytes()); // longs per entry
        indx.extend_from_slice(&[0, 0]); // sub type, AVI_INDEX_OF_INDEXES
        indx.extend_from_slice(&(self.super_index.len() as u32).to_le_bytes());
        indx.extend_from_slice(b"00dc");
        indx.extend_from_slice(&[0u8; 12]);
        for &(off, size, dur) in &self.super_index {
            indx.extend_from_slice(&off.to_le_bytes());
            indx.extend_from_slice(&size.to_le_bytes());
            indx.extend_from_slice(&dur.to_le_bytes());
        }
        self.f.seek(SeekFrom::Start(self.indx_start))?;
        self.f.write_all(&indx)?;
        if self.audio_rate.is_some() {
            let blocks: u32 = self
                .parts
                .iter()
                .flat_map(|p| p.audio.iter())
                .map(|a| a.1 / 2)
                .sum();
            let mut indx = vec![];
            indx.extend_from_slice(&4u16.to_le_bytes());
            indx.extend_from_slice(&[0, 0]);
            indx.extend_from_slice(&(self.audio_super_index.len() as u32).to_le_bytes());
            indx.extend_from_slice(b"01wb");
            indx.extend_from_slice(&[0u8; 12]);
            for &(off, size, dur) in &self.audio_super_index {
                indx.extend_from_slice(&off.to_le_bytes());
                indx.extend_from_slice(&size.to_le_bytes());
                indx.extend_from_slice(&dur.to_le_bytes());
            }
            self.f.seek(SeekFrom::Start(self.audio_indx_start))?;
            self.f.write_all(&indx)?;
            self.f.seek(SeekFrom::Start(self.audio_strh_length))?;
            self.f.write_all(&blocks.to_le_bytes())?;
            self.f.write_all(&self.max_audio.to_le_bytes())?;
        }
        self.f.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_into_avix_parts() {
        let dir = std::env::temp_dir().join(format!("videorec_avi_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.avi");
        let mut w =
            AviWriter::with_limit(&p, *b"H264", 320, 240, 30, 1, 64 * 1024, Some(8000)).unwrap();
        for i in 0..100u32 {
            w.add_frame(&vec![i as u8; 1001 + i as usize], i % 10 == 0)
                .unwrap();
            w.add_audio(&[0u8; 534]).unwrap();
        }
        w.finish().unwrap();
        let b = std::fs::read(&p).unwrap();
        assert_eq!(&b[0..4], b"RIFF");
        let first = u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize + 8;
        assert_eq!(&b[first..first + 4], b"RIFF");
        assert_eq!(&b[first + 8..first + 12], b"AVIX");
        if let Ok(keep) = std::env::var("VIDEOREC_KEEP_AVI") {
            std::fs::copy(&p, keep).unwrap();
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
