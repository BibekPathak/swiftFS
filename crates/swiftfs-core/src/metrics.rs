//! Lightweight, allocation-free counters and latency histograms.
//!
//! These are intentionally simple: atomics plus fixed-bucket histograms. They
//! avoid unbounded label cardinality (no per-path labels) and can be rendered to
//! a Prometheus text format without a heavy dependency or the Prometheus crate.
//!
//! Histogram buckets are in microseconds.

use std::sync::atomic::{AtomicU64, Ordering};

/// Histogram bucket upper bounds in microseconds.
pub const LATENCY_BUCKETS_US: &[u64] = &[
    50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 25_000, 50_000, 100_000, 250_000, 500_000,
    1_000_000, 5_000_000,
];

/// A monotonically increasing counter.
#[derive(Debug, Default)]
pub struct Counter(AtomicU64);

impl Counter {
    /// Increment by one.
    pub fn inc(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment by `n`.
    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    /// Current value.
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// A value that can go up or down (e.g. resident bytes).
#[derive(Debug, Default)]
pub struct Gauge(AtomicU64);

impl Gauge {
    /// Set the value.
    pub fn set(&self, v: u64) {
        self.0.store(v, Ordering::Relaxed);
    }

    /// Increment by `n`.
    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    /// Decrement by `n`, saturating at zero.
    pub fn sub(&self, n: u64) {
        let _ = self
            .0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                Some(cur.saturating_sub(n))
            });
    }

    /// Current value.
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// A fixed-bucket latency histogram plus sum and count.
#[derive(Debug)]
pub struct Histogram {
    /// Count per bucket (`<=` upper bound), plus a final `+Inf` bucket.
    buckets: Vec<AtomicU64>,
    sum_us: AtomicU64,
    count: AtomicU64,
}

impl Default for Histogram {
    fn default() -> Self {
        let mut buckets = Vec::with_capacity(LATENCY_BUCKETS_US.len() + 1);
        for _ in 0..=LATENCY_BUCKETS_US.len() {
            buckets.push(AtomicU64::new(0));
        }
        Self {
            buckets,
            sum_us: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }
}

impl Histogram {
    /// Record a latency in microseconds.
    pub fn observe_us(&self, us: u64) {
        let idx = LATENCY_BUCKETS_US
            .iter()
            .position(|&b| us <= b)
            .unwrap_or(LATENCY_BUCKETS_US.len());
        self.buckets[idx].fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add(us, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a latency measured as a `Duration`.
    pub fn observe_duration(&self, d: std::time::Duration) {
        self.observe_us(d.as_micros() as u64);
    }

    /// Total observations.
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// Sum of observed latencies in microseconds.
    pub fn sum_us(&self) -> u64 {
        self.sum_us.load(Ordering::Relaxed)
    }

    /// Bucket values with their upper bounds; the last entry is `+Inf` (`None`).
    pub fn buckets(&self) -> impl Iterator<Item = (Option<u64>, u64)> + '_ {
        self.buckets.iter().enumerate().map(|(i, c)| {
            let bound = LATENCY_BUCKETS_US.get(i).copied();
            (bound, c.load(Ordering::Relaxed))
        })
    }
}

/// Format a histogram in Prometheus text exposition format, with cumulative
/// buckets as Prometheus requires.
pub fn write_histogram(
    out: &mut String,
    name: &str,
    help: &str,
    hist: &Histogram,
) {
    out.push_str(&format!("# HELP {name} {help}\n"));
    out.push_str(&format!("# TYPE {name} histogram\n"));
    let mut cumulative = 0u64;
    for (bound, value) in hist.buckets() {
        cumulative += value;
        match bound {
            Some(b) => {
                let secs = b as f64 / 1_000_000.0;
                out.push_str(&format!("{name}_bucket{{le=\"{secs}\"}} {cumulative}\n"));
            }
            None => {
                out.push_str(&format!("{name}_bucket{{le=\"+Inf\"}} {cumulative}\n"));
            }
        }
    }
    out.push_str(&format!("{name}_sum {}\n", hist.sum_us() as f64 / 1_000_000.0));
    out.push_str(&format!("{name}_count {}\n", hist.count()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_accumulates() {
        let c = Counter::default();
        c.inc();
        c.add(4);
        assert_eq!(c.get(), 5);
    }

    #[test]
    fn histogram_buckets_are_cumulative() {
        let h = Histogram::default();
        h.observe_us(10); // <= 50
        h.observe_us(200); // <= 250
        h.observe_us(10_000_000); // +Inf
        assert_eq!(h.count(), 3);

        let mut out = String::new();
        write_histogram(&mut out, "x_latency_seconds", "test", &h);
        assert!(out.contains("x_latency_seconds_count 3"));
        // Cumulative: first bucket has 1, and +Inf has 3.
        assert!(out.contains("le=\"+Inf\"} 3"));
    }
}
