//! Saved scan sessions: where recorder-format data was found, without any
//! video, so clips can be exported later (with other filters or formats)
//! without scanning the whole source again.

use crate::io::Source;
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::Path;

pub const MAGIC: &str = "videorec-session-1";

#[derive(serde::Serialize, serde::Deserialize)]
pub struct JdrVolume {
    pub offset: u64,
    pub end: u64,
    /// Byte runs made of back-to-back chunks.
    pub runs: Vec<(u64, u64)>,
    /// (start, end) of live `.jdr` files, for earlier-pass marks.
    pub files: Vec<(u64, u64)>,
    pub chunks: usize,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Session {
    pub format: String,
    pub tool: String,
    pub source: String,
    pub source_size: u64,
    /// SHA-256 of the first and last MiB of the source.
    pub fingerprint: String,
    pub jdr: Vec<JdrVolume>,
    /// Byte runs made of back-to-back Dahua (DHAV) frames.
    #[serde(default)]
    pub dvr: Vec<(u64, u64)>,
}

pub fn fingerprint(src: &Source) -> Result<String> {
    let mut h = Sha256::new();
    let n = src.len().min(1 << 20);
    h.update(src.read_vec(0, n as usize)?);
    h.update(src.read_vec(src.len() - n, n as usize)?);
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

impl Session {
    pub fn new(
        src: &Source,
        image: &Path,
        jdr: Vec<JdrVolume>,
        dvr: Vec<(u64, u64)>,
    ) -> Result<Self> {
        Ok(Session {
            format: MAGIC.into(),
            tool: format!("videorec {}", env!("CARGO_PKG_VERSION")),
            source: image.display().to_string(),
            source_size: src.len(),
            fingerprint: fingerprint(src)?,
            jdr,
            dvr,
        })
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        std::fs::write(path, serde_json::to_vec(self)?)
            .with_context(|| format!("cannot write session {}", path.display()))
    }

    pub fn load(path: &Path) -> Result<Self> {
        let s: Session = serde_json::from_slice(
            &std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?,
        )
        .with_context(|| format!("{} is not a videorec session", path.display()))?;
        anyhow::ensure!(s.format == MAGIC, "unsupported session format {}", s.format);
        Ok(s)
    }

    /// Fails unless `src` looks like the source the session was made from.
    pub fn check(&self, src: &Source) -> Result<()> {
        anyhow::ensure!(
            src.len() == self.source_size && fingerprint(src)? == self.fingerprint,
            "source does not match the session (size or content differs from {})",
            self.source
        );
        Ok(())
    }
}
