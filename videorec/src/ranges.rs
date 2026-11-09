//! Sorted, merged set of half-open byte ranges.

#[derive(Debug, Clone, Default)]
pub struct Ranges {
    v: Vec<(u64, u64)>,
    sorted: bool,
}

impl Ranges {
    pub fn new() -> Self {
        Self {
            v: vec![],
            sorted: true,
        }
    }

    pub fn add(&mut self, start: u64, end: u64) {
        if end <= start {
            return;
        }
        if let Some(last) = self.v.last_mut() {
            if self.sorted && start == last.1 {
                last.1 = end;
                return;
            }
            if start < last.0 {
                self.sorted = false;
            }
        }
        self.v.push((start, end));
    }

    pub fn normalize(&mut self) {
        self.v.sort_unstable();
        let mut out: Vec<(u64, u64)> = Vec::with_capacity(self.v.len());
        for &(s, e) in &self.v {
            match out.last_mut() {
                Some(l) if s <= l.1 => l.1 = l.1.max(e),
                _ => out.push((s, e)),
            }
        }
        self.v = out;
        self.sorted = true;
    }

    /// Range containing `pos`, if any. Requires `normalize()`.
    pub fn containing(&self, pos: u64) -> Option<(u64, u64)> {
        let i = self.v.partition_point(|r| r.1 <= pos);
        self.v.get(i).copied().filter(|r| r.0 <= pos)
    }

    /// Start of the first range at or after `pos`, or `u64::MAX`.
    pub fn next_start(&self, pos: u64) -> u64 {
        let i = self.v.partition_point(|r| r.1 <= pos);
        self.v.get(i).map(|r| r.0.max(pos)).unwrap_or(u64::MAX)
    }

    /// Splits `[start, end)` into the sub-ranges NOT covered by self.
    pub fn gaps(&self, start: u64, end: u64) -> Vec<(u64, u64)> {
        let mut out = vec![];
        let mut p = start;
        while p < end {
            if let Some((_, e)) = self.containing(p) {
                p = e;
                continue;
            }
            let n = self.next_start(p).min(end);
            out.push((p, n));
            p = n;
        }
        out
    }

    pub fn total(&self) -> u64 {
        self.v.iter().map(|r| r.1 - r.0).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gaps_work() {
        let mut r = Ranges::new();
        r.add(10, 20);
        r.add(30, 40);
        r.add(15, 25);
        r.normalize();
        assert_eq!(r.gaps(0, 50), vec![(0, 10), (25, 30), (40, 50)]);
        assert_eq!(r.containing(24), Some((10, 25)));
        assert_eq!(r.containing(25), None);
    }
}
