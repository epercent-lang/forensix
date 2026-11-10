//! videorec — recovers deleted video from dashcam SD cards and CCTV disks.
//!
//! The source is opened read-only. Recovery runs in three stages:
//! 1. file-system metadata (deleted FAT32/exFAT entries),
//!    then recorder-specific chunk formats (G-NET `.jdr`) over the whole volume,
//! 2. container carving (intact MP4/MOV/AVI),
//! 3. frame-level carving of H.264/H.265 streams in what is left.

mod avi;
mod codec;
mod container;
mod fs;
mod gui;
mod io;
mod jdr;
mod mjpeg;
mod mp4;
mod ntfs;
mod progress;
mod ranges;
mod recorder;
mod report;
mod session;
mod stream;

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
    },
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
    std::fs::create_dir_all(out).with_context(|| format!("cannot create {}", out.display()))?;
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
    let summary = serde_json::json!({
        "source": image.display().to_string(), "source_size": src.len(),
        "session": session_path.display().to_string(),
        "jdr_chunks": total, "jdr_runs_not_matching_session": broken,
        "jdr_clips_from_earlier_pass": older, "jdr_segments_discarded": discarded,
        "jdr_by_month": stats,
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

fn volumes(src: &Source) -> Result<Vec<Volume>> {
    let mut v = io::find_volumes(src)?;
    if v.is_empty() {
        v.push(Volume {
            offset: 0,
            length: src.len(),
            kind: FsKind::Unknown,
        });
    }
    Ok(v)
}

fn copy_range(src: &Source, start: u64, end: u64, path: &Path) -> Result<()> {
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
    std::fs::create_dir_all(out).with_context(|| format!("cannot create {}", out.display()))?;
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

    for v in volumes(&src)? {
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
                    for &(a, b) in &d.extents {
                        f.write_all(&src.read_vec(a, (b - a) as usize)?)?;
                    }
                    rec.output = path.file_name().unwrap().to_string_lossy().into();
                    rec.status = "partial".into();
                    rec.notes = format!("{} ({} extents); not verified", d.method, d.extents.len());
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
    if let Some(p) = &clip_extra.save_session {
        session::Session::new(&src, image, session_vols)?.save(p)?;
        eprintln!("session saved: {}", p.display());
    }
    if !jdr_vols.is_empty() {
        summary.insert("jdr_chunks".into(), jdr_chunks_total.into());
        summary.insert("jdr_clips_from_earlier_pass".into(), jdr_older.into());
        summary.insert("jdr_segments_discarded".into(), jdr_discarded.into());
        summary.insert("jdr_by_month".into(), serde_json::to_value(&jdr_stats)?);
    }

    // Stage 2: intact containers in the scanned regions.
    let mut damaged_containers = 0usize;
    if !no_containers {
        for &(a, b) in &regions {
            for (s, e) in covered.clone().gaps(a, b) {
                let mut found = |c: container::Container| -> Result<u64> {
                    if !c.intact {
                        damaged_containers += 1;
                        return Ok(0);
                    }
                    let id = items.len() + 1;
                    let path = out.join(format!("carved_{id:05}_{:012x}.{}", c.start, c.kind));
                    copy_range(&src, c.start, c.end, &path)?;
                    covered.add(c.start, c.end);
                    items.push(Recovered {
                        id,
                        kind: "carved_container".into(),
                        output: path.file_name().unwrap().to_string_lossy().into(),
                        sha256: report::sha256_file(&path)?,
                        size: c.end - c.start,
                        source_start: c.start,
                        source_end: c.end,
                        codec: c.config.as_ref().map(|x| x.codec.name().into()),
                        width: c
                            .config
                            .as_ref()
                            .and_then(|x| x.sps_info())
                            .map(|i| i.width),
                        height: c
                            .config
                            .as_ref()
                            .and_then(|x| x.sps_info())
                            .map(|i| i.height),
                        duration_s: c.duration_s,
                        status: "ok".into(),
                        notes: "container structure verified".into(),
                        ..Default::default()
                    });
                    Ok(c.end)
                };
                container::scan(&src, s, e, &mut configs, &mut found)?;
            }
        }
        covered.normalize();
    }

    // Stage 3: frame-level carving of whatever is not yet covered.
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
    let mut discarded = 0;
    let mut chains_without_params = 0;
    if !no_streams {
        let mut carver = stream::Carver::new(&src, &opts, &configs, &hints, out, items.len() + 1);
        for &(a, b) in &regions {
            for (s, e) in covered.gaps(a, b) {
                carver.scan_region(s, e)?;
            }
        }
        carver.flush()?;
        discarded = carver.discarded_small;
        chains_without_params = carver.chains_without_params;
        items.extend(carver.results);
    }

    // Stage 3b: Motion-JPEG runs in what is still not covered.
    let mut mjpeg_runs_small = 0;
    if !no_streams {
        let mut cov = covered.clone();
        for it in &items {
            cov.add(it.source_start, it.source_end);
        }
        cov.normalize();
        let mut jpegs = vec![];
        for &(a, b) in &regions {
            for (s, e) in cov.gaps(a, b) {
                mjpeg::scan(&src, s, e, &mut jpegs)?;
            }
        }
        let (got, small) = mjpeg::write_runs(
            &src,
            &jpegs,
            out,
            items.len() + 1,
            max_gap_kib * 1024,
            min_frames,
            fps,
        )?;
        mjpeg_runs_small = small;
        items.extend(got);
    }

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
        "video_chains_without_parameter_sets".into(),
        chains_without_params.into(),
    );
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
    match cli.cmd {
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
            },
            max_gap_kib,
            min_frames,
            keep_leading,
            no_fs,
            no_containers,
            no_streams,
        ),
    }
}
