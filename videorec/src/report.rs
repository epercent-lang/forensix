//! Recovery report (JSON + CSV) and output hashing.

use anyhow::Result;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Recovered {
    pub id: usize,
    /// fs_deleted_file | carved_container | carved_stream
    pub kind: String,
    pub output: String,
    pub sha256: String,
    pub size: u64,
    pub source_start: u64,
    pub source_end: u64,
    pub codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub frames: Option<usize>,
    pub keyframes: Option<usize>,
    pub duration_s: Option<f64>,
    /// Number of separate on-disk pieces the stream was assembled from.
    pub pieces: Option<usize>,
    pub name_hint: Option<String>,
    pub modified: Option<String>,
    pub created: Option<String>,
    pub config_source: Option<String>,
    /// ok | partial | needs_parameter_sets | damaged
    pub status: String,
    pub notes: String,
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

pub fn write_csv(path: &Path, items: &[Recovered]) -> Result<()> {
    let mut f = File::create(path)?;
    writeln!(
        f,
        "id,kind,status,output,sha256,size,source_start,source_end,codec,width,height,frames,keyframes,duration_s,pieces,name_hint,modified,created,config_source,notes"
    )?;
    let o = |v: Option<String>| v.unwrap_or_default();
    for r in items {
        let row = [
            r.id.to_string(),
            r.kind.clone(),
            r.status.clone(),
            r.output.clone(),
            r.sha256.clone(),
            r.size.to_string(),
            format!("{:#x}", r.source_start),
            format!("{:#x}", r.source_end),
            o(r.codec.clone()),
            o(r.width.map(|v| v.to_string())),
            o(r.height.map(|v| v.to_string())),
            o(r.frames.map(|v| v.to_string())),
            o(r.keyframes.map(|v| v.to_string())),
            o(r.duration_s.map(|v| format!("{v:.2}"))),
            o(r.pieces.map(|v| v.to_string())),
            o(r.name_hint.clone()),
            o(r.modified.clone()),
            o(r.created.clone()),
            o(r.config_source.clone()),
            r.notes.clone(),
        ];
        let line: Vec<String> = row.iter().map(|s| csv_field(s)).collect();
        writeln!(f, "{}", line.join(","))?;
    }
    Ok(())
}
