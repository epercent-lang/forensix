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
        self.done = self.total;
        if !self.quiet {
            self.print();
            eprintln!();
        }
    }
}
