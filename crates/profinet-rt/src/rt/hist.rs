//! Fixed-bin latency histogram, written by the RT thread (one relaxed `fetch_add`
//! and one `fetch_max` per sample) and read from any other thread.

use std::sync::atomic::{AtomicU64, Ordering};

/// 1 µs bins from 0 to 2046 µs; the last bin collects everything ≥ 2047 µs.
pub const HIST_BINS: usize = 2048;

/// Plain-value copy of a [`Histogram`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistSnapshot {
    /// Sample count per 1 µs bin; `bins[HIST_BINS - 1]` is the overflow bin.
    pub bins: Vec<u64>,
    pub count: u64,
    pub max_ns: u64,
}

pub struct Histogram {
    bins: [AtomicU64; HIST_BINS],
    count: AtomicU64,
    max_ns: AtomicU64,
}

impl Histogram {
    pub const fn new() -> Self {
        // Used only to repeat-initialize the array below, never as a shared instance:
        // the interior-mutability lint's concern (aliased const state) doesn't apply.
        #[allow(clippy::declare_interior_mutable_const)]
        const ZERO: AtomicU64 = AtomicU64::new(0);
        Self {
            bins: [ZERO; HIST_BINS],
            count: AtomicU64::new(0),
            max_ns: AtomicU64::new(0),
        }
    }

    /// Record one sample in nanoseconds. RT-safe: no lock, no allocation.
    pub fn record(&self, ns: u64) {
        let bin = usize::try_from(ns / 1000).map_or(HIST_BINS - 1, |b| b.min(HIST_BINS - 1));
        self.bins[bin].fetch_add(1, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.max_ns.fetch_max(ns, Ordering::Relaxed);
    }

    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    pub fn max_ns(&self) -> u64 {
        self.max_ns.load(Ordering::Relaxed)
    }

    /// The bin (in µs) below which `p` percent of the samples fall (`p` in `0..=100`);
    /// `None` when empty. The overflow bin reads as `HIST_BINS - 1` — use
    /// [`Histogram::max_ns`] for the real maximum.
    pub fn percentile(&self, p: f64) -> Option<u64> {
        let count = self.count();
        if count == 0 {
            return None;
        }
        let p = p.clamp(0.0, 100.0);
        // Rank of the wanted sample, 1-based; p = 0 → the first sample.
        let target = ((p / 100.0) * count as f64).ceil().max(1.0) as u64;
        let mut seen = 0u64;
        for (i, bin) in self.bins.iter().enumerate() {
            seen += bin.load(Ordering::Relaxed);
            if seen >= target {
                return Some(i as u64);
            }
        }
        Some((HIST_BINS - 1) as u64) // counts raced past `count`: report the tail
    }

    pub fn snapshot(&self) -> HistSnapshot {
        HistSnapshot {
            bins: self
                .bins
                .iter()
                .map(|b| b.load(Ordering::Relaxed))
                .collect(),
            count: self.count(),
            max_ns: self.max_ns(),
        }
    }

    pub fn reset(&self) {
        for b in &self.bins {
            b.store(0, Ordering::Relaxed);
        }
        self.count.store(0, Ordering::Relaxed);
        self.max_ns.store(0, Ordering::Relaxed);
    }
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Histogram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Histogram")
            .field("count", &self.count())
            .field("max_ns", &self.max_ns())
            .field("p50_us", &self.percentile(50.0))
            .field("p99_us", &self.percentile(99.0))
            .field("p9999_us", &self.percentile(99.99))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_histogram_has_no_percentile() {
        let h = Histogram::new();
        assert_eq!(h.count(), 0);
        assert_eq!(h.max_ns(), 0);
        assert_eq!(h.percentile(50.0), None);
    }

    #[test]
    fn dirac_reports_its_bin_at_every_percentile() {
        let h = Histogram::new();
        for _ in 0..100 {
            h.record(42_300); // 42.3 µs → bin 42
        }
        assert_eq!(h.count(), 100);
        assert_eq!(h.max_ns(), 42_300);
        assert_eq!(h.percentile(0.0), Some(42));
        assert_eq!(h.percentile(50.0), Some(42));
        assert_eq!(h.percentile(99.99), Some(42));
        assert_eq!(h.percentile(100.0), Some(42));
    }

    #[test]
    fn uniform_distribution_percentiles() {
        let h = Histogram::new();
        for us in 0..1000u64 {
            h.record(us * 1000);
        }
        assert_eq!(h.percentile(50.0), Some(499));
        assert_eq!(h.percentile(99.0), Some(989));
        assert_eq!(h.percentile(99.99), Some(999));
        assert_eq!(h.percentile(100.0), Some(999));
    }

    #[test]
    fn overflow_goes_to_the_last_bin_and_max_keeps_the_real_value() {
        let h = Histogram::new();
        h.record(5_000_000); // 5 ms
        assert_eq!(h.percentile(100.0), Some((HIST_BINS - 1) as u64));
        assert_eq!(h.max_ns(), 5_000_000);
        assert_eq!(h.snapshot().bins[HIST_BINS - 1], 1);
    }

    #[test]
    fn reset_clears_everything() {
        let h = Histogram::new();
        h.record(10_000);
        h.reset();
        assert_eq!(h.count(), 0);
        assert_eq!(h.max_ns(), 0);
        assert_eq!(h.percentile(50.0), None);
        assert!(h.snapshot().bins.iter().all(|&b| b == 0));
    }
}
