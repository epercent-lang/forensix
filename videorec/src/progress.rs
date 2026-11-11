//! Progress line on stderr: percentage, elapsed and estimated remaining time.

use std::io::Write;
use std::time::{Duration, Instant};

pub struct Progress {
    label: String,
    total: u64,
    done: u64,
    start: Instant,
    last: Instant,
    quiet: bool,
}

fn hms(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

impl Progress {
    pub fn new(label: &str, total: u64, quiet: bool) -> Self {
        let now = Instant::now();
        Progress {
            label: label.into(),
            total: total.max(1),
            done: 0,
            start: now,
            last: now,
            quiet,
        }
    }

    pub fn add(&mut self, n: u64) {
        self.done = (self.done + n).min(self.total);
        if !self.quiet && self.last.elapsed() >= Duration::from_secs(1) {
            self.last = Instant::now();
            self.print();
        }
    }

    fn print(&self) {
        let el = self.start.elapsed();
        let frac = self.done as f64 / self.total as f64;
        let left = if frac > 0.0 {
            hms(el.mul_f64((1.0 - frac) / frac))
        } else {
            "?".into()
        };
        eprint!(
            "\r[{}] {:5.1}%  {:.1}/{:.1} GB  elapsed {}  remaining {}   ",
            self.label,
            frac * 100.0,
            self.done as f64 / 1e9,
            self.total as f64 / 1e9,
            hms(el),
            left
        );
        std::io::stderr().flush().ok();
    }

    pub fn finish(mut self) {
        if !STOP.load(std::sync::atomic::Ordering::Relaxed) {
            self.done = self.total;
        }
        if !self.quiet {
            self.print();
            eprintln!();
        }
    }
}

// ------------------------------------------------------------ stages, stop

use std::path::PathBuf;
use std::sync::Mutex;

struct Stage {
    p: Progress,
    /// Bytes finished in earlier regions, and the current region's start.
    base: u64,
    region: u64,
}

static STAGE: Mutex<Option<Stage>> = Mutex::new(None);
static STOP_FILE: Mutex<Option<PathBuf>> = Mutex::new(None);
static STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static LAST_CHECK: Mutex<Option<Instant>> = Mutex::new(None);

/// Starts a scan stage over `total` bytes (ends the previous one).
pub fn stage(label: &str, total: u64, quiet: bool) {
    let mut s = STAGE.lock().unwrap();
    if let Some(old) = s.take() {
        old.p.finish();
    }
    *s = Some(Stage {
        p: Progress::new(label, total, quiet),
        base: 0,
        region: 0,
    });
}

/// The stage moves on to a region starting at `start` (after `done` bytes).
pub fn region(start: u64, done: u64) {
    if let Some(s) = STAGE.lock().unwrap().as_mut() {
        s.base = done;
        s.region = start;
    }
}

/// Current position inside the region.
pub fn at(pos: u64) {
    if let Some(s) = STAGE.lock().unwrap().as_mut() {
        let d = s.base + pos.saturating_sub(s.region);
        if d > s.p.done {
            let n = d - s.p.done;
            s.p.add(n);
        }
    }
}

pub fn end_stage() {
    if let Some(old) = STAGE.lock().unwrap().take() {
        old.p.finish();
    }
}

/// A file whose appearance asks the scan to stop early (the GUI's stop button).
pub fn set_stop_file(p: PathBuf) {
    std::fs::remove_file(&p).ok();
    *STOP_FILE.lock().unwrap() = Some(p);
}

/// True once a stop was requested; scans then end early and the report is
/// written with what was found so far.
pub fn stopped() -> bool {
    use std::sync::atomic::Ordering;
    if STOP.load(Ordering::Relaxed) {
        return true;
    }
    let mut last = LAST_CHECK.lock().unwrap();
    if last.is_some_and(|t| t.elapsed() < Duration::from_millis(500)) {
        return false;
    }
    *last = Some(Instant::now());
    if let Some(p) = STOP_FILE.lock().unwrap().as_ref()
        && p.exists()
    {
        STOP.store(true, Ordering::Relaxed);
        eprintln!("\nstop requested: finishing with what was found so far");
        return true;
    }
    false
}

// ------------------------------------------------- file system analysis

static FS_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static FS: Mutex<Option<Progress>> = Mutex::new(None);

/// Turns the progress line for file system analysis on (scans only).
pub fn show_fs(on: bool) {
    FS_ON.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Position of the file system analysis: `done` of `total` bytes of
/// metadata read. Reaching `total` ends the line.
pub fn fs(done: u64, total: u64) {
    if !FS_ON.load(std::sync::atomic::Ordering::Relaxed) || total == 0 {
        return;
    }
    let mut g = FS.lock().unwrap();
    let p = g.get_or_insert_with(|| {
        let p = Progress::new("fs", total, false);
        p.print();
        p
    });
    if done >= total {
        if let Some(p) = g.take() {
            p.finish();
        }
    } else if done > p.done {
        let n = done - p.done;
        p.add(n);
    }
}
