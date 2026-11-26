//! Detects disks written by CCTV recorders with their own file systems.
//! Only signatures from published papers are checked; the file systems are
//! not parsed yet, so video on such disks is found by frame carving.
//!
//! Sources:
//! - Hikvision: Han, Jeong, Lee, "Analysis of the HIKVISION DVR File System",
//!   ICDF2C 2015, LNICST 157, DOI 10.1007/978-3-319-25512-5_13
//!   (master sector "HIKVISION@HANGZHOU" at 0x200, index "HIKBTREE").
//! - Dahua: Rzayeva et al., Information 16(11):983, 2025,
//!   DOI 10.3390/info16110983 ("DHFS4.1" looked for at 512, 1024, 2048;
//!   also at 0, where a DH-HCVR5216AN-S2 disk has it).
//! - Honeywell: Yoon, Hwang, DFRWS USA 2026 (machine data at 0x4400,
//!   "sn private disk" in the paper's hex figure).

use crate::io::Source;
use anyhow::Result;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Hit {
    pub system: &'static str,
    pub signature: &'static str,
    pub offset: u64,
    pub source: &'static str,
}

const HIK: &str = "Han et al., ICDF2C 2015";
const DAHUA: &str = "Rzayeva et al., Information 2025 (offsets not verified on a real disk)";
const HONEYWELL: &str = "Yoon & Hwang, DFRWS USA 2026 (not verified on a real disk)";

fn find(buf: &[u8], pat: &[u8]) -> Option<usize> {
    buf.windows(pat.len()).position(|w| w == pat)
}

/// Looks at the first 64 MiB of the source.
pub fn detect(src: &Source) -> Result<Vec<Hit>> {
    let n = src.len().min(64 << 20) as usize;
    let mut head = vec![0u8; n];
    src.read_exact_at(0, &mut head)?;
    let mut hits = vec![];
    let at = |o: usize, p: &[u8]| head.len() >= o + p.len() && &head[o..o + p.len()] == p;
    // Offset 0 too: a 4 TB Dahua DH-HCVR5216AN-S2 disk has "DHFS4.1" at 0
    // (checked on the user's disk, 2026-11-10).
    for o in [0usize, 0x200, 0x400, 0x800] {
        if at(o, b"HIKVISION@HANGZHOU") {
            hits.push(Hit {
                system: "Hikvision DVR/NVR",
                signature: "HIKVISION@HANGZHOU",
                offset: o as u64,
                source: HIK,
            });
        }
        if at(o, b"DHFS4.1") {
            hits.push(Hit {
                system: "Dahua DHFS4.1",
                signature: "DHFS4.1",
                offset: o as u64,
                source: DAHUA,
            });
        }
    }
    if !hits.iter().any(|h| h.system.starts_with("Hikvision"))
        && let Some(o) = find(&head, b"HIKBTREE")
    {
        hits.push(Hit {
            system: "Hikvision DVR/NVR",
            signature: "HIKBTREE",
            offset: o as u64,
            source: HIK,
        });
    }
    if at(0x4400, b"sn private disk") {
        hits.push(Hit {
            system: "Honeywell NVR",
            signature: "sn private disk",
            offset: 0x4400,
            source: HONEYWELL,
        });
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    #[test]
    fn finds_signature() {
        assert_eq!(super::find(b"xxHIKBTREEyy", b"HIKBTREE"), Some(2));
    }
}
