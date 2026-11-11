//! videorec — recovers deleted video from dashcam SD cards and CCTV disks.
//!
//! The source is opened read-only. Recovery runs in three stages:
//! 1. file-system metadata (deleted FAT32/exFAT entries),
//!    then recorder-specific chunk formats (G-NET `.jdr`) over the whole volume,
//! 2. container carving (intact MP4/MOV/AVI),
//! 3. frame-level carving of H.264/H.265 streams in what is left.

mod apfs;
mod avi;
mod btrfs;
mod codec;
mod container;
mod dhav;
mod ext4;
mod fs;
mod gui;
mod hfsplus;
mod io;
mod jdr;
mod jfs;
mod mj2k;
mod mjpeg;
mod mp4;
mod mp4fix;
mod mpeg12;
mod mpeg4v;
mod multi;
mod ntfs;
mod preview;
mod progress;
mod ranges;
mod recorder;
mod reiser;
mod report;
mod session;
mod stream;
mod xfs;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use codec::Codec;
use io::{FsKind, Source, Volume};
use ranges::Ranges;
use report::Recovered;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    version,
    about = "Recover deleted video from dashcam SD cards and CCTV disks (read-only)."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum, PartialEq)]
enum CodecArg {
    Auto,
    H264,
    H265,
}

#[derive(Clone, Copy, ValueEnum, PartialEq)]
enum Scope {
    /// Free space when the file system is recognised, otherwise everything.
    Auto,
    /// Only clusters the file system marks as free.
    Free,
    /// The whole volume, including allocated clusters (for recorders that
    /// pre-allocate files and overwrite inside them).
    All,
}

#[derive(Clone, Copy, ValueEnum, PartialEq)]
enum Vendor {
    /// Use a recorder format when the card's files show it (e.g. `*.jdr`).
    Auto,
    /// Always scan for G-NET `.jdr` chunks.
    Jdr,
    /// Always scan for Dahua DVR/NVR frames (DHAV).
    Dahua,
    /// Generic carving only.
    None,
}

#[derive(Clone, Copy, ValueEnum, PartialEq)]
enum FormatArg {
    Mp4,
    Avi,
}

#[derive(Clone, Copy, ValueEnum, PartialEq)]
enum GroupArg {
    /// All files in the output folder.
    None,
    /// CAM<n>/ sub-folders.
    Camera,
    /// YYYY-MM-DD/ sub-folders.
    Date,
    /// CAM<n>/YYYY-MM-DD/ sub-folders.
    CameraDate,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the local GUI (opens a browser page on 127.0.0.1).
    Gui {
        /// Port on 127.0.0.1 (0 picks a free one).
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Only print the address; do not open a browser.
        #[arg(long)]
        no_browser: bool,
    },
    /// Show partitions, file systems and deleted entries.
    Info { image: PathBuf },
    /// List every file (live and deleted) with size, times and on-disk extents, as CSV.
    List {
        image: PathBuf,
        /// Write CSV here instead of stdout.
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Copy one file (live or deleted, by path as shown by `list`) out of the image.
    Extract {
        image: PathBuf,
        /// Path inside the image, e.g. /DCIM/REC0001.AVI
        path: String,
        #[arg(short, long)]
        out: PathBuf,
        /// Copy at most this many bytes from the start of the file.
        #[arg(long)]
        max_bytes: Option<u64>,
    },
    /// Copy a raw byte range of the source (image or disk) to a file, for
    /// examining an unknown recorder format. Reads only.
    Dump {
        image: PathBuf,
        /// Start, in bytes (0x… for hex). Rounded down to a 512-byte sector.
        #[arg(long, default_value = "0")]
        offset: String,
        /// Length with optional K/M/G suffix (binary units), e.g. 64M.
        #[arg(long, default_value = "64M")]
        len: String,
        #[arg(short, long)]
        out: PathBuf,
    },
    /// Write clips from a session saved with `recover --save-session`,
    /// without scanning the source again (recorder formats only).
    Export {
        session: PathBuf,
        #[arg(short, long)]
        out: PathBuf,
        /// Source image or disk, if it moved since the session was saved.
        #[arg(long)]
        image: Option<PathBuf>,
        #[arg(long, default_value_t = 2000)]
        max_gap_ms: i64,
        #[arg(long, default_value_t = 1)]
        jdr_min_frames: usize,
        #[arg(long)]
        jdr_drop_without_key: bool,
        #[arg(long)]
        stats_only: bool,
        #[arg(long, value_enum, default_value_t = FormatArg::Mp4)]
        format: FormatArg,
        #[arg(long, value_enum, default_value_t = GroupArg::None)]
        group: GroupArg,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        camera: Option<String>,
        #[arg(long, short)]
        quiet: bool,
        /// Leave the recorder's audio out of rebuilt clips.
        #[arg(long)]
        no_audio: bool,
    },
    /// Recover video into an output directory.
    Recover {
        image: PathBuf,
        #[arg(short, long)]
        out: PathBuf,
        /// Healthy recording from the same device; supplies SPS/PPS and frame rate.
        #[arg(short, long)]
        reference: Vec<PathBuf>,
        #[arg(long, value_enum, default_value_t = CodecArg::Auto)]
        codec: CodecArg,
        /// Override frame rate for re-wrapped streams.
        #[arg(long)]
        fps: Option<f64>,
        #[arg(long, value_enum, default_value_t = Scope::Auto)]
        scope: Scope,
        /// Recorder-specific format handling.
        #[arg(long, value_enum, default_value_t = Vendor::Auto)]
        vendor: Vendor,
        /// Largest time gap (ms) between frames still treated as one clip
        /// (recorder formats with timestamps).
        #[arg(long, default_value_t = 2000)]
        max_gap_ms: i64,
        /// Only count what recorder formats hold (per month in report.json); write no video.
        #[arg(long)]
        stats_only: bool,
        /// Discard recorder-format clips with fewer frames.
        #[arg(long, default_value_t = 1)]
        jdr_min_frames: usize,
        /// Drop recorder-format segments in which no key frame survived
        /// (by default they are kept, marked partial, with borrowed SPS/PPS).
        #[arg(long)]
        jdr_drop_without_key: bool,
        /// Container for clips rebuilt from recorder formats.
        #[arg(long, value_enum, default_value_t = FormatArg::Mp4)]
        format: FormatArg,
        /// Sub-folders for clips rebuilt from recorder formats.
        #[arg(long, value_enum, default_value_t = GroupArg::None)]
        group: GroupArg,
        /// Keep only recordings from this time on: YYYY-MM-DD or "YYYY-MM-DD hh:mm[:ss]"
        /// (recorder clock; applies to recorder formats with timestamps).
        #[arg(long)]
        from: Option<String>,
        /// Keep only recordings up to this time (same format as --from).
        #[arg(long)]
        to: Option<String>,
        /// Keep only these cameras, e.g. 1,3 or 1-2 (camera = recorder channel + 1).
        #[arg(long)]
        camera: Option<String>,
        /// No progress line.
        #[arg(long, short)]
        quiet: bool,
        /// Leave the recorder's audio out of rebuilt clips.
        #[arg(long)]
        no_audio: bool,
        /// Save what the scan found (positions only, no video) to this file,
        /// for `export` later without scanning again. Keep it off the source disk.
        #[arg(long)]
        save_session: Option<PathBuf>,
        /// Largest gap (KiB) between NAL runs still treated as one stream
        /// (interleaved audio, metadata).
        #[arg(long, default_value_t = 1024)]
        max_gap_kib: u64,
        /// Discard carved streams with fewer frames.
        #[arg(long, default_value_t = 10)]
        min_frames: usize,
        /// Keep frames before the first keyframe (not decodable on their own).
        #[arg(long)]
        keep_leading: bool,
        #[arg(long)]
        no_fs: bool,
        #[arg(long)]
        no_containers: bool,
        #[arg(long)]
        no_streams: bool,
        /// Skip the H.264/H.265 frame pass.
        #[arg(long)]
        no_h26x: bool,
        /// Skip the Motion-JPEG pass (one more full read of the source).
        #[arg(long)]
        no_mjpeg: bool,
        /// Skip the MPEG-4 Part 2 pass (one more full read of the source).
        #[arg(long)]
        no_mpeg4: bool,
        /// Skip the MPEG-1/2 search.
        #[arg(long)]
        no_mpeg12: bool,
        /// Skip the Motion JPEG 2000 search.
        #[arg(long)]
        no_mj2k: bool,
        /// Carve only this part of the source, in GB (10^9 bytes) from the
        /// start: e.g. 0-100, or 3500- for the rest of the disk.
        #[arg(long)]
        range_gb: Option<String>,
        /// Scan only these volumes (partitions), numbered from 1 in the
        /// order `info` lists them: e.g. 2 or 1,3. Default: all.
        #[arg(long)]
        volume: Option<String>,
    },
}

/// Parses `A-B` / `A-` in GB into a byte range.
fn parse_range_gb(s: &str, len: u64) -> Result<(u64, u64)> {
    let (a, b) = s
        .split_once('-')
        .context("--range-gb expects START-END in GB, e.g. 0-100")?;
    let gb = |t: &str| -> Result<u64> {
        let v: f64 = t
            .trim()
            .parse()
            .with_context(|| format!("not a number: {t}"))?;
        anyhow::ensure!(v >= 0.0, "negative position");
        Ok((v * 1e9) as u64)
    };
    let a = gb(a)?.min(len);
    let b = if b.trim().is_empty() {
        len
    } else {
        gb(b)?.min(len)
    };
    anyhow::ensure!(a < b, "empty range {s}");
    // Whole sectors.
    Ok((a & !511, (b.div_ceil(512) * 512).min(len)))
}

/// Output and filter options for clips rebuilt from recorder formats.
struct ClipExtra {
    format: FormatArg,
    group: GroupArg,
    from: Option<String>,
    to: Option<String>,
    camera: Option<String>,
    quiet: bool,
    save_session: Option<PathBuf>,
    no_audio: bool,
    no_h26x: bool,
    no_mjpeg: bool,
    no_mpeg4: bool,
    no_mpeg12: bool,
    no_mj2k: bool,
    range_gb: Option<String>,
    volume: Option<String>,
}

fn clip_opts(
    x: &ClipExtra,
    max_gap_ms: i64,
    min_frames: usize,
    drop_without_key: bool,
    stats_only: bool,
) -> Result<jdr::ClipOpts> {
    let parse_t = |v: &Option<String>, end: bool| -> Result<Option<i64>> {
        match v {
            None => Ok(None),
            Some(t) => jdr::parse_time(t, end).map(Some).with_context(|| {
                format!("bad time {t:?}; use YYYY-MM-DD or \"YYYY-MM-DD hh:mm:ss\"")
            }),
        }
    };
    Ok(jdr::ClipOpts {
        max_gap_ms,
        min_frames,
        keep_without_key: !drop_without_key,
        dry_run: stats_only,
        format: match x.format {
            FormatArg::Mp4 => jdr::Format::Mp4,
            FormatArg::Avi => jdr::Format::Avi,
        },
        group: match x.group {
            GroupArg::None => jdr::Group::None,
            GroupArg::Camera => jdr::Group::Camera,
            GroupArg::Date => jdr::Group::Date,
            GroupArg::CameraDate => jdr::Group::CameraDate,
        },
        from_ms: parse_t(&x.from, false)?,
        to_ms: parse_t(&x.to, true)?,
        cameras: match &x.camera {
            None => vec![],
            Some(c) => jdr::parse_cameras(c)
                .with_context(|| format!("bad camera list {c:?}; use e.g. 1,3 or 1-2"))?,
        },
        audio: !x.no_audio,
    })
}

/// Exports clips from a saved session without scanning again.
fn export(
    session_path: &Path,
    image: Option<&Path>,
    out: &Path,
    o: &jdr::ClipOpts,
    quiet: bool,
) -> Result<()> {
    let ses = session::Session::load(session_path)?;
    let image = image
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(&ses.source));
    let src = Source::open(&image)?;
    ses.check(&src)?;
    guard_output(&image, out)?;
    std::fs::create_dir_all(out).with_context(|| format!("cannot create {}", out.display()))?;
    progress::set_stop_file(out.join(".stop"));
    let mut items = vec![];
    let mut stats = jdr::Stats::new();
    let (mut discarded, mut older, mut total, mut broken) = (0, 0, 0, 0);
    for v in &ses.jdr {
        let mut chunks = Vec::with_capacity(v.chunks);
        let mut prog =
            progress::Progress::new("read", v.runs.iter().map(|r| r.1 - r.0).sum(), quiet);
        broken += jdr::walk(&src, &v.runs, &mut chunks, &mut prog)?;
        prog.finish();
        total += chunks.len();
        let (got, d, ol) = jdr::clips_with_pass_marks(
            &src,
            &chunks,
            out,
            items.len() + 1,
            o,
            &mut stats,
            &v.files,
        )?;
        discarded += d;
        older += ol;
        items.extend(got);
    }
    let mut dvr = serde_json::Value::Null;
    if !ses.dvr.is_empty() {
        progress::stage("read", ses.dvr.iter().map(|r| r.1 - r.0).sum(), quiet);
        let mut cov = Ranges::new();
        let mut st = dhav::Stats::default();
        let (chunks, older_frames) = dhav::scan(&src, &ses.dvr, &mut cov, &mut st)?;
        progress::end_stage();
        let mut by_month = jdr::Stats::new();
        let (mut got, d) = jdr::write_clips(&src, &chunks, out, items.len() + 1, o, &mut by_month)?;
        let mut old = 0;
        for r in got.iter_mut() {
            if older_frames.contains(&r.source_start) {
                old += 1;
                r.notes.push_str("; older footage: found after newer recording in the same 2 MiB area of the disk, i.e. left over where the recorder has since written");
            }
        }
        dvr = serde_json::json!({"dvr_frames": st, "dvr_clips_older_footage": old,
            "dvr_segments_discarded": d, "dvr_by_month": by_month});
        items.extend(got);
    }
    let summary = serde_json::json!({
        "source": image.display().to_string(), "source_size": src.len(),
        "session": session_path.display().to_string(), "dvr": dvr,
        "jdr_chunks": total, "jdr_runs_not_matching_session": broken,
        "jdr_clips_from_earlier_pass": older, "jdr_segments_discarded": discarded,
        "jdr_by_month": stats,
        "stopped_early": progress::stopped(),
        "unreadable_sectors": src.bad_sectors(),
        "tool": format!("videorec {}", env!("CARGO_PKG_VERSION")),
    });
    std::fs::write(
        out.join("report.json"),
        serde_json::to_string_pretty(&serde_json::json!({"summary": summary, "items": items}))?,
    )?;
    report::write_csv(&out.join("report.csv"), &items)?;
    println!(
        "{} items; report: {}",
        items.len(),
        out.join("report.json").display()
    );
    Ok(())
}

fn parse_size(s: &str) -> Result<u64> {
    let t = s.trim();
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return Ok(u64::from_str_radix(h, 16)?);
    }
    let (num, mul) = match t.chars().last().map(|c| c.to_ascii_uppercase()) {
        Some('K') => (&t[..t.len() - 1], 1u64 << 10),
        Some('M') => (&t[..t.len() - 1], 1 << 20),
        Some('G') => (&t[..t.len() - 1], 1 << 30),
        _ => (t, 1),
    };
    Ok(num
        .trim()
        .parse::<u64>()
        .with_context(|| format!("not a size: {s}"))?
        * mul)
}

fn dump(image: &Path, offset: &str, len: &str, out: &Path) -> Result<()> {
    let src = Source::open(image)?;
    guard_output(image, out.parent().unwrap_or(Path::new(".")))?;
    let start = parse_size(offset)? & !511;
    let len = parse_size(len)?.min(4 << 30);
    let end = (start + len).min(src.len());
    anyhow::ensure!(start < end, "offset is past the end ({} bytes)", src.len());
    copy_range(&src, start, end, out)?;
    println!(
        "{} bytes from {start:#x} written to {} (sha256 {})",
        end - start,
        out.display(),
        report::sha256_file(out)?
    );
    Ok(())
}

/// Refuses to write recovered files onto the physical disk being recovered:
/// new files would overwrite the deleted data we are trying to read.
fn guard_output(image: &Path, out: &Path) -> Result<()> {
    let Some(n) = io::device_number(image).filter(|_| io::is_device_path(image)) else {
        return Ok(());
    };
    let s = out.to_string_lossy();
    let letter = s
        .chars()
        .next()
        .filter(|c| c.is_ascii_alphabetic() && s[1..].starts_with(':'));
    match letter {
        Some(c) if io::disk_of_letter(c.to_ascii_uppercase()) == Some(n) => anyhow::bail!(
            "저장 위치 {}가 복구 대상 디스크 {n}에 있습니다. 덮어쓰기를 막기 위해 다른 디스크를 고르세요",
            out.display()
        ),
        None if !out.is_absolute() && cfg!(windows) => {
            let cwd = std::env::current_dir()?;
            guard_output(image, &cwd.join(out))
        }
        _ => Ok(()),
    }
}

fn volumes(src: &Source) -> Result<Vec<Volume>> {
    let mut v = io::find_volumes(src)?;
    if v.is_empty() {
        v.push(Volume {
            offset: 0,
            length: src.len(),
            kind: FsKind::Unknown,
        });
        return Ok(v);
    }
    // Space the partition table does not cover (before, between and after
    // the partitions) is scanned too, as volumes without a file system:
    // recorders often keep their data outside any partition. Listed after
    // the partitions so that their numbers stay the same.
    let mut used: Vec<(u64, u64)> = v.iter().map(|x| (x.offset, x.offset + x.length)).collect();
    used.sort();
    let mut pos = 0u64;
    for (a, b) in used.into_iter().chain([(src.len(), src.len())]) {
        if a > pos && a - pos >= 1 << 20 {
            let mut boot = [0u8; 512];
            let kind = match src.read_exact_at(pos, &mut boot) {
                Ok(()) if pos > 0 => io::detect_at(src, pos, &boot),
                _ => FsKind::Unknown,
            };
            v.push(Volume {
                offset: pos,
                length: a - pos,
                kind,
            });
        }
        pos = pos.max(b);
    }
    Ok(v)
}

pub(crate) fn copy_range(src: &Source, start: u64, end: u64, path: &Path) -> Result<()> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut p = start;
    let mut buf = vec![0u8; 4 << 20];
    while p < end {
        let n = buf.len().min((end - p) as usize);
        let got = src.read_at(p, &mut buf[..n])?;
        if got == 0 {
            break;
        }
        f.write_all(&buf[..got])?;
        p += got as u64;
    }
    f.flush()?;
    Ok(())
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn info(image: &Path) -> Result<()> {
    let src = Source::open(image)?;
    println!("source: {} ({} bytes)", image.display(), src.len());
    for h in recorder::detect(&src)? {
        println!(
            "recorder disk: {} ({:?} at {:#x}; source: {}). Its file system is not parsed yet; `recover` finds the video by frame carving.",
            h.system, h.signature, h.offset, h.source
        );
    }
    for v in volumes(&src)? {
        println!("volume @ {:#x}, {} bytes, {:?}", v.offset, v.length, v.kind);
        if let Some(fsi) = fs::analyze(&src, &v)? {
            println!(
                "  cluster {} bytes, {} clusters, {} live files, {} deleted entries, {} bytes allocated",
                fsi.cluster_size,
                fsi.cluster_count,
                fsi.live_files,
                fsi.deleted.len(),
                fsi.allocated.total()
            );
            for d in &fsi.deleted {
                println!(
                    "  deleted: {} ({} bytes, {}, {:?})",
                    d.path, d.size, d.method, d.modified
                );
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn recover(
    image: &Path,
    out: &Path,
    references: &[PathBuf],
    codec_arg: CodecArg,
    fps: Option<f64>,
    scope: Scope,
    vendor: Vendor,
    max_gap_ms: i64,
    stats_only: bool,
    jdr_min_frames: usize,
    jdr_drop_without_key: bool,
    clip_extra: ClipExtra,
    max_gap_kib: u64,
    min_frames: usize,
    keep_leading: bool,
    no_fs: bool,
    no_containers: bool,
    no_streams: bool,
) -> Result<()> {
    let src = Source::open(image)?;
    guard_output(image, out)?;
    std::fs::create_dir_all(out).with_context(|| format!("cannot create {}", out.display()))?;
    // A report left by an earlier run into the same folder would describe
    // that run if this one does not finish: keep it under another name.
    for f in ["report.json", "report.csv", "videorec_error.txt"] {
        let p = out.join(f);
        if p.exists() {
            std::fs::rename(&p, out.join(format!("previous_{f}"))).ok();
        }
    }
    progress::set_stop_file(out.join(".stop"));
    let mut items: Vec<Recovered> = vec![];
    let mut configs = vec![];
    for r in references {
        match mp4::read_reference(r)? {
            Some(c) => configs.push(c),
            None => eprintln!(
                "warning: no H.264/H.265 track found in reference {}",
                r.display()
            ),
        }
    }
    let mut covered = Ranges::new();
    let mut hints = vec![];
    let mut regions: Vec<(u64, u64)> = vec![];
    let mut summary = serde_json::Map::new();
    let mut vol_info = vec![];
    // (start, end, offset of start within the file, path) of live files.
    let mut live_map: Vec<(u64, u64, u64, String)> = vec![];
    // Volumes to scan for `.jdr` chunks, and the data start of each live `.jdr` file.
    let mut jdr_vols: Vec<(u64, u64)> = vec![];
    let mut jdr_files: Vec<(u64, u64)> = vec![];

    let only: Option<Vec<usize>> = match &clip_extra.volume {
        Some(s) => Some(
            s.split(',')
                .map(|x| x.trim().parse::<usize>())
                .collect::<std::result::Result<_, _>>()
                .map_err(|_| anyhow::anyhow!("--volume takes numbers such as 2 or 1,3"))?,
        ),
        None => None,
    };
    let all_volumes = volumes(&src)?;
    if let Some(o) = &only
        && let Some(bad) = o.iter().find(|&&n| n == 0 || n > all_volumes.len())
    {
        anyhow::bail!(
            "--volume {bad}: the source has {} volume(s)",
            all_volumes.len()
        );
    }
    for (vi, v) in all_volumes.into_iter().enumerate() {
        if only.as_ref().is_some_and(|o| !o.contains(&(vi + 1))) {
            vol_info.push(serde_json::json!({
                "offset": v.offset, "length": v.length, "fs": format!("{:?}", v.kind),
                "skipped": true,
            }));
            continue;
        }
        let fsi = if no_fs {
            None
        } else {
            fs::analyze(&src, &v).unwrap_or_else(|e| {
                eprintln!("warning: {e}");
                None
            })
        };
        let vend = v.offset + v.length;
        vol_info.push(serde_json::json!({
            "offset": v.offset, "length": v.length, "fs": format!("{:?}", v.kind),
            "deleted_entries": fsi.as_ref().map(|f| f.deleted.len()),
            "live_files": fsi.as_ref().map(|f| f.live_files),
        }));
        match (&fsi, scope) {
            (Some(f), Scope::Auto | Scope::Free) => {
                regions.extend(f.allocated.gaps(v.offset, vend))
            }
            (None, Scope::Free) => eprintln!(
                "warning: volume @ {:#x} has no recognised file system; skipped (--scope free)",
                v.offset
            ),
            _ => regions.push((v.offset, vend)),
        }
        let has_jdr = fsi.as_ref().is_some_and(|f| {
            f.live
                .iter()
                .chain(&f.deleted)
                .any(|x| x.path.to_ascii_lowercase().ends_with(".jdr"))
        });
        if vendor == Vendor::Jdr || (vendor == Vendor::Auto && has_jdr) {
            jdr_vols.push((v.offset, vend));
        }
        let Some(fsi) = fsi else { continue };
        for f in &fsi.live {
            if f.path.to_ascii_lowercase().ends_with(".jdr")
                && let (Some(&(a, _)), Some(&(_, b))) = (f.extents.first(), f.extents.last())
            {
                jdr_files.push((a, b));
            }
            let mut base = 0u64;
            for &(a, b) in &f.extents {
                live_map.push((a, b, base, f.path.clone()));
                base += b - a;
            }
        }
        for d in &fsi.deleted {
            hints.push(stream::Hint {
                extents: d.extents.clone(),
                name: d.path.clone(),
                modified: d.modified.clone(),
                created: d.created.clone(),
            });
            let id = items.len() + 1;
            let mut rec = Recovered {
                id,
                kind: "fs_deleted_file".into(),
                name_hint: Some(d.path.clone()),
                modified: d.modified.clone(),
                created: d.created.clone(),
                ..Default::default()
            };
            if d.extents.is_empty() {
                rec.status = "damaged".into();
                rec.notes = format!("{}: data start already reused by another file", d.method);
                items.push(rec);
                continue;
            }
            let (s, e) = (d.extents[0].0, d.extents.last().unwrap().1);
            rec.source_start = s;
            rec.source_end = e;
            let name = sanitize(d.path.rsplit('/').next().unwrap_or("file"));
            let single = d.extents.len() == 1;
            let check = if single {
                match container::check_mp4(&src, s, e)? {
                    Some(c) => Some(c),
                    None => container::check_avi(&src, s, e)?,
                }
            } else {
                None
            };
            if let Some(cf) = check.as_ref().and_then(|c| c.config.clone())
                && !configs
                    .iter()
                    .any(|x: &mp4::VideoConfig| x.key() == cf.key())
            {
                configs.push(cf);
            }
            match check {
                Some(c) if c.intact && c.end == e => {
                    let path = out.join(format!("fs_{id:05}_{name}"));
                    copy_range(&src, s, e, &path)?;
                    covered.add(s, e);
                    rec.output = path.file_name().unwrap().to_string_lossy().into();
                    rec.status = "ok".into();
                    rec.notes = format!("{}; container structure verified", d.method);
                }
                Some(c) => {
                    rec.status = "damaged".into();
                    rec.notes = format!(
                        "{}; {} — left to frame carving",
                        d.method,
                        if c.note.is_empty() {
                            "size mismatch".into()
                        } else {
                            c.note
                        }
                    );
                }
                None if !single => {
                    // Clusters skipped around allocated ones; copy but cannot verify.
                    let path = out.join(format!("fs_{id:05}_{name}"));
                    let mut f = std::fs::File::create(&path)?;
                    let mut buf = vec![0u8; 4 << 20];
                    for &(a, b) in &d.extents {
                        let mut p = a;
                        while p < b {
                            let n = src.read_at(p, &mut buf[..((b - p) as usize).min(4 << 20)])?;
                            if n == 0 {
                                break;
                            }
                            f.write_all(&buf[..n])?;
                            p += n as u64;
                        }
                    }
                    drop(f);
                    rec.output = path.file_name().unwrap().to_string_lossy().into();
                    // The pieces put together: is the result a sound file?
                    let whole = Source::open(&path)?;
                    let sound = match container::check_mp4(&whole, 0, whole.len())? {
                        Some(c) => c.intact && c.end == whole.len(),
                        None => container::check_avi(&whole, 0, whole.len())?
                            .is_some_and(|c| c.intact && c.end == whole.len()),
                    };
                    if sound {
                        for &(a, b) in &d.extents {
                            covered.add(a, b);
                        }
                        rec.status = "ok".into();
                        rec.notes = format!(
                            "{} ({} extents); container structure verified",
                            d.method,
                            d.extents.len()
                        );
                    } else {
                        rec.status = "partial".into();
                        rec.notes =
                            format!("{} ({} extents); not verified", d.method, d.extents.len());
                    }
                }
                None => {
                    rec.status = "damaged".into();
                    rec.notes = format!(
                        "{}; no MP4/AVI header at the recorded start — overwritten or not a video",
                        d.method
                    );
                }
            }
            if !rec.output.is_empty() {
                let p = out.join(&rec.output);
                rec.size = std::fs::metadata(&p)?.len();
                rec.sha256 = report::sha256_file(&p)?;
            }
            items.push(rec);
        }
    }
    covered.normalize();

    // Stage 1b: recorder chunk formats, scanned over the whole volume because
    // the recorder overwrites inside pre-allocated files.
    let mut jdr_discarded = 0usize;
    let mut jdr_chunks_total = 0usize;
    let mut jdr_older = 0usize;
    let mut jdr_stats = jdr::Stats::new();
    let clip_opts = clip_opts(
        &clip_extra,
        max_gap_ms,
        jdr_min_frames,
        jdr_drop_without_key,
        stats_only,
    )?;
    let (no_containers, no_streams) = (no_containers || stats_only, no_streams || stats_only);
    let mut session_vols = vec![];
    for &(a, b) in &jdr_vols {
        let mut chunks = vec![];
        let mut cov = Ranges::new();
        let gaps = covered.gaps(a, b);
        let mut prog = progress::Progress::new(
            "scan",
            gaps.iter().map(|g| g.1 - g.0).sum(),
            clip_extra.quiet,
        );
        for (s, e) in gaps {
            jdr::scan(&src, s, e, &mut chunks, &mut cov, &mut prog)?;
        }
        prog.finish();
        jdr_chunks_total += chunks.len();
        let (got, d, older) = jdr::clips_with_pass_marks(
            &src,
            &chunks,
            out,
            items.len() + 1,
            &clip_opts,
            &mut jdr_stats,
            &jdr_files,
        )?;
        jdr_discarded += d;
        jdr_older += older;
        cov.normalize();
        session_vols.push(session::JdrVolume {
            offset: a,
            end: b,
            runs: cov.iter().collect(),
            files: jdr_files.clone(),
            chunks: chunks.len(),
        });
        items.extend(got);
        for (s, e) in cov.iter() {
            covered.add(s, e);
        }
    }
    covered.normalize();
    if !jdr_vols.is_empty() {
        summary.insert("jdr_chunks".into(), jdr_chunks_total.into());
        summary.insert("jdr_clips_from_earlier_pass".into(), jdr_older.into());
        summary.insert("jdr_segments_discarded".into(), jdr_discarded.into());
        summary.insert("jdr_by_month".into(), serde_json::to_value(&jdr_stats)?);
    }

    if let Some(r) = &clip_extra.range_gb {
        let (a, b) = parse_range_gb(r, src.len())?;
        regions = regions
            .iter()
            .filter_map(|&(s, e)| (s.max(a) < e.min(b)).then_some((s.max(a), e.min(b))))
            .collect();
        summary.insert("carve_range".into(), serde_json::json!([a, b]));
    }

    // Dahua DVR/NVR frames: camera and time in every frame header.
    let mut session_dvr: Vec<(u64, u64)> = vec![];
    let dahua_disk = recorder::detect(&src)?
        .iter()
        .any(|h| h.system.starts_with("Dahua"));
    if vendor == Vendor::Dahua || (vendor == Vendor::Auto && dahua_disk) {
        let gaps: Vec<(u64, u64)> = regions
            .iter()
            .flat_map(|&(a, b)| covered.clone().gaps(a, b))
            .collect();
        progress::stage(
            "dvr",
            gaps.iter().map(|g| g.1 - g.0).sum(),
            clip_extra.quiet,
        );
        let mut cov = Ranges::new();
        let mut st = dhav::Stats::default();
        let (chunks, older_frames) = dhav::scan(&src, &gaps, &mut cov, &mut st)?;
        progress::end_stage();
        let mut by_month = jdr::Stats::new();
        let (mut got, discarded) = jdr::write_clips(
            &src,
            &chunks,
            out,
            items.len() + 1,
            &clip_opts,
            &mut by_month,
        )?;
        let mut older = 0;
        for r in got.iter_mut() {
            if older_frames.contains(&r.source_start) {
                older += 1;
                r.notes.push_str("; older footage: found after newer recording in the same 2 MiB area of the disk, i.e. left over where the recorder has since written");
            }
        }
        summary.insert("dvr_frames".into(), serde_json::to_value(&st)?);
        summary.insert("dvr_clips_older_footage".into(), older.into());
        summary.insert("dvr_segments_discarded".into(), discarded.into());
        summary.insert("dvr_by_month".into(), serde_json::to_value(&by_month)?);
        items.extend(got);
        cov.normalize();
        session_dvr = cov.iter().collect();
        for (s, e) in cov.iter() {
            covered.add(s, e);
        }
        covered.normalize();
    }
    if let Some(p) = &clip_extra.save_session {
        session::Session::new(&src, image, session_vols, session_dvr)?.save(p)?;
        eprintln!("session saved: {}", p.display());
    }

    // Stages 2 and 3 in one read: intact containers, H.264/H.265 streams,
    // Motion-JPEG and MPEG-4 Part 2 in what is not yet covered (multi.rs).
    let opts = stream::Options {
        codecs: match codec_arg {
            CodecArg::Auto => vec![Codec::H264, Codec::H265],
            CodecArg::H264 => vec![Codec::H264],
            CodecArg::H265 => vec![Codec::H265],
        },
        fps,
        max_gap: max_gap_kib * 1024,
        min_frames,
        keep_leading,
    };
    let mut want = multi::Want {
        containers: !no_containers,
        h26x: !no_streams && !clip_extra.no_h26x,
        mjpeg: !no_streams && !clip_extra.no_mjpeg,
        mpeg4: !no_streams && !clip_extra.no_mpeg4,
        mpeg12: !no_streams && !clip_extra.no_mpeg12,
        mj2k: !no_streams && !clip_extra.no_mj2k,
    };
    let mut damaged_containers = 0usize;
    let mut salvaged_containers = 0usize;
    let (mut discarded, mut chains_without_params) = (0, 0);
    let (mut mjpeg_runs_small, mut mpeg4_runs_small, mut mpeg12_runs_small) = (0, 0, 0);
    let mut mj2k_runs_small = 0;
    let mut read_passes = 0;
    let read_before = src.bytes_read();
    let cache_mib = std::env::var("VIDEOREC_CACHE_MB")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(512);
    while want.containers || want.h26x || want.mjpeg || want.mpeg4 || want.mpeg12 || want.mj2k {
        src.set_block_cache(cache_mib);
        let claimed: Vec<(u64, u64)> = items
            .iter()
            .map(|i| (i.source_start, i.source_end))
            .collect();
        let params = multi::Params {
            src: &src,
            out,
            regions: &regions,
            opts: &opts,
            hints: &hints,
            ids: std::rc::Rc::new(std::cell::Cell::new(items.len() + 1)),
            quiet: clip_extra.quiet,
        };
        let got = multi::pass(&params, want, &mut covered, &claimed, &mut configs)?;
        src.set_block_cache(0);
        read_passes += 1;
        damaged_containers += got.damaged_containers;
        salvaged_containers += got.salvaged_containers;
        discarded += got.discarded_small;
        chains_without_params += got.chains_without_params;
        mjpeg_runs_small += got.mjpeg_small;
        mpeg4_runs_small += got.mpeg4_small;
        mpeg12_runs_small += got.mpeg12_small;
        mj2k_runs_small += got.mj2k_small;
        items.extend(got.items);
        if !got.redo || progress::stopped() {
            break;
        }
        // Parameter sets turned up after they were needed: the containers
        // are done, carve again knowing all of them.
        want.containers = false;
    }
    summary.insert("read_passes".into(), read_passes.into());
    summary.insert("mj2k_runs_below_min_frames".into(), mj2k_runs_small.into());
    summary.insert(
        "mpeg12_runs_below_min_frames".into(),
        mpeg12_runs_small.into(),
    );
    summary.insert("containers_kept_in_part".into(), salvaged_containers.into());
    summary.insert(
        "carving_bytes_read".into(),
        (src.bytes_read() - read_before).into(),
    );

    live_map.sort_unstable_by_key(|x| x.0);
    for it in items.iter_mut() {
        let i = live_map.partition_point(|x| x.1 <= it.source_start);
        if let Some((a, _, base, p)) = live_map.get(i).filter(|x| x.0 <= it.source_start) {
            it.live_file = Some(p.clone());
            it.live_file_offset = Some(base + (it.source_start - a));
        }
    }
    summary.insert(
        "items_inside_live_files".into(),
        items
            .iter()
            .filter(|i| i.live_file.is_some())
            .count()
            .into(),
    );
    summary.insert("source".into(), image.display().to_string().into());
    summary.insert("source_size".into(), src.len().into());
    summary.insert("volumes".into(), vol_info.into());
    summary.insert(
        "recorder_disk".into(),
        serde_json::to_value(recorder::detect(&src)?)?,
    );
    summary.insert(
        "scanned_bytes".into(),
        regions.iter().map(|r| r.1 - r.0).sum::<u64>().into(),
    );
    summary.insert(
        "configs".into(),
        configs
            .iter()
            .map(|c| {
                serde_json::json!({"codec": c.codec.name(), "origin": c.origin, "fps": c.fps,
            "width": c.sps_info().map(|i| i.width), "height": c.sps_info().map(|i| i.height)})
            })
            .collect::<Vec<_>>()
            .into(),
    );
    summary.insert(
        "damaged_containers_left_to_carving".into(),
        damaged_containers.into(),
    );
    summary.insert("small_fragments_discarded".into(), discarded.into());
    summary.insert("jpeg_runs_below_min_frames".into(), mjpeg_runs_small.into());
    summary.insert(
        "mpeg4_runs_below_min_frames".into(),
        mpeg4_runs_small.into(),
    );
    summary.insert(
        "video_chains_without_parameter_sets".into(),
        chains_without_params.into(),
    );
    progress::end_stage();
    summary.insert("unreadable_sectors".into(), src.bad_sectors().into());
    summary.insert("stopped_early".into(), progress::stopped().into());
    summary.insert(
        "tool".into(),
        format!("videorec {}", env!("CARGO_PKG_VERSION")).into(),
    );
    let report = serde_json::json!({"summary": summary, "items": items});
    std::fs::write(
        out.join("report.json"),
        serde_json::to_string_pretty(&report)?,
    )?;
    report::write_csv(&out.join("report.csv"), &items)?;

    if chains_without_params > 0 {
        eprintln!(
            "note: {chains_without_params} H.264/H.265 frame sequences were found but no SPS/PPS to decode them; \
             re-run with --reference <healthy file from the same device>"
        );
    }
    let ok = items.iter().filter(|i| i.status == "ok").count();
    let partial = items.iter().filter(|i| i.status == "partial").count();
    println!(
        "{} items ({} ok, {} partial, {} other); report: {}",
        items.len(),
        ok,
        partial,
        items.len() - ok - partial,
        out.join("report.json").display()
    );
    Ok(())
}

fn all_files(src: &Source) -> Result<Vec<(bool, fs::DeletedFile)>> {
    let mut v = vec![];
    for vol in volumes(src)? {
        if let Some(f) = fs::analyze(src, &vol)? {
            v.extend(f.live.into_iter().map(|x| (true, x)));
            v.extend(f.deleted.into_iter().map(|x| (false, x)));
        }
    }
    Ok(v)
}

fn list(image: &Path, out: Option<&Path>) -> Result<()> {
    let src = Source::open(image)?;
    let mut w: Box<dyn Write> = match out {
        Some(p) => Box::new(std::io::BufWriter::new(std::fs::File::create(p)?)),
        None => Box::new(std::io::stdout().lock()),
    };
    writeln!(
        w,
        "state,path,size,modified,created,first_cluster,extents,first_offset,last_end,method"
    )?;
    for (live, f) in all_files(&src)? {
        let (a, b) = match (f.extents.first(), f.extents.last()) {
            (Some(a), Some(b)) => (format!("{:#x}", a.0), format!("{:#x}", b.1)),
            _ => (String::new(), String::new()),
        };
        writeln!(
            w,
            "{},\"{}\",{},{},{},{},{},{},{},{}",
            if live { "live" } else { "deleted" },
            f.path.replace('"', "\"\""),
            f.size,
            f.modified.unwrap_or_default(),
            f.created.unwrap_or_default(),
            f.first_cluster,
            f.extents.len(),
            a,
            b,
            f.method
        )?;
    }
    Ok(())
}

fn extract(image: &Path, path: &str, out: &Path, max_bytes: Option<u64>) -> Result<()> {
    let src = Source::open(image)?;
    let files = all_files(&src)?;
    let Some((_, f)) = files
        .iter()
        .find(|(_, f)| f.path.eq_ignore_ascii_case(path))
    else {
        anyhow::bail!("{path} not found; use `list` to see paths");
    };
    let mut left = max_bytes.unwrap_or(u64::MAX).min(f.size);
    let mut w = std::io::BufWriter::new(std::fs::File::create(out)?);
    for &(a, b) in &f.extents {
        if left == 0 {
            break;
        }
        let n = (b - a).min(left);
        let mut p = a;
        let mut buf = vec![0u8; 4 << 20];
        while p < a + n {
            let k = buf.len().min((a + n - p) as usize);
            let got = src.read_at(p, &mut buf[..k])?;
            if got == 0 {
                break;
            }
            w.write_all(&buf[..got])?;
            p += got as u64;
        }
        left -= n;
    }
    w.flush()?;
    println!("wrote {} ({} extents)", out.display(), f.extents.len());
    Ok(())
}

fn main() -> Result<()> {
    // Started with no arguments (e.g. double-clicked): open the GUI.
    if std::env::args_os().len() == 1 {
        return gui::run(0, true);
    }
    let cli = Cli::parse();
    let err_out = match &cli.cmd {
        Cmd::Export { out, .. } | Cmd::Recover { out, .. } => Some(out.clone()),
        _ => None,
    };
    let res = match cli.cmd {
        Cmd::Export {
            session,
            out,
            image,
            max_gap_ms,
            jdr_min_frames,
            jdr_drop_without_key,
            stats_only,
            format,
            group,
            from,
            to,
            camera,
            quiet,
            no_audio,
        } => {
            let x = ClipExtra {
                format,
                group,
                from,
                to,
                camera,
                quiet,
                save_session: None,
                no_audio,
                no_h26x: false,
                no_mjpeg: false,
                no_mpeg4: false,
                no_mpeg12: false,
                no_mj2k: false,
                range_gb: None,
                volume: None,
            };
            let o = clip_opts(
                &x,
                max_gap_ms,
                jdr_min_frames,
                jdr_drop_without_key,
                stats_only,
            )?;
            export(&session, image.as_deref(), &out, &o, quiet)
        }
        Cmd::Gui { port, no_browser } => gui::run(port, !no_browser),
        Cmd::Info { image } => info(&image),
        Cmd::List { image, out } => list(&image, out.as_deref()),
        Cmd::Extract {
            image,
            path,
            out,
            max_bytes,
        } => extract(&image, &path, &out, max_bytes),
        Cmd::Dump {
            image,
            offset,
            len,
            out,
        } => dump(&image, &offset, &len, &out),
        Cmd::Recover {
            image,
            out,
            reference,
            codec,
            fps,
            scope,
            vendor,
            max_gap_ms,
            stats_only,
            jdr_min_frames,
            jdr_drop_without_key,
            format,
            group,
            from,
            to,
            camera,
            quiet,
            no_audio,
            save_session,
            max_gap_kib,
            min_frames,
            keep_leading,
            no_fs,
            no_containers,
            no_streams,
            no_h26x,
            no_mjpeg,
            no_mpeg4,
            no_mpeg12,
            no_mj2k,
            range_gb,
            volume,
        } => recover(
            &image,
            &out,
            &reference,
            codec,
            fps,
            scope,
            vendor,
            max_gap_ms,
            stats_only,
            jdr_min_frames,
            jdr_drop_without_key,
            ClipExtra {
                format,
                group,
                from,
                to,
                camera,
                quiet,
                save_session,
                no_audio,
                no_h26x,
                no_mjpeg,
                no_mpeg4,
                no_mpeg12,
                no_mj2k,
                range_gb,
                volume,
            },
            max_gap_kib,
            min_frames,
            keep_leading,
            no_fs,
            no_containers,
            no_streams,
        ),
    };
    if let (Err(e), Some(out)) = (&res, &err_out) {
        // The progress line ends with '\r': start the message on a new line,
        // and leave it next to the results for later.
        eprintln!();
        let msg = format!("{e:#}");
        if out.is_dir() {
            std::fs::write(out.join("videorec_error.txt"), format!("{msg}\n")).ok();
        }
    }
    res
}
