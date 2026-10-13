//! Benchmark harness for SwiftFS.
//!
//! This module produces **real** measurements. It does not fabricate numbers.
//! Workloads either time SwiftFS image operations (`build`, `inspect`,
//! `verify`, `mount` preparation) or perform genuine chunk reads through the
//! same `ChunkFetcher` (RAM + disk caches, single-flight coalescing) that the
//! FUSE read path uses.
//!
//! # What is measured
//!
//! - `build` / `inspect` / `verify`: wall-clock time of the corresponding
//!   image operations.
//! - `mount`: time to load and validate the manifest and construct the
//!   immutable `ManifestIndex` — the work `mount` does before the FUSE session
//!   starts. This is deliberately *not* reported as application startup time.
//! - `read` (cold) / `warm_read` (warm) / `compare` / `read-ahead` / `io`:
//!   latency samples from real chunk reads over the local backend, with a
//!   controlled cache state. p50/p99 are computed from those samples.
//!
//! # What is avoided
//!
//! Mounting a FUSE filesystem is not attempted from inside this harness; FUSE
//! availability depends on the host. Read latency is therefore measured at the
//! `ChunkFetcher` layer, which is the layer that dominates a SwiftFS read. The
//! Python `bench_suite.py` can additionally drive a real mount when `/dev/fuse`
//! is available.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use swiftfs_cache::{ChunkFetcher, FetcherConfig, RamCache};
use swiftfs_format::{build_image, inspect_image, load_manifest, verify_image, BuildOptions};
use swiftfs_storage::{ChunkLocation, ChunkStore, LocalStore};

use crate::index::ManifestIndex;

/// Configuration for a benchmark run.
#[derive(Debug, Clone)]
pub struct BenchConfig {
    /// Number of measured iterations per workload (after warm-up).
    pub iterations: usize,
    /// Enable read-ahead for read workloads. When set, the harness issues the
    /// same sequential chunk reads the FUSE path would trigger; the effect is
    /// visible through cache-hit rates across iterations.
    pub read_ahead: bool,
    /// RAM cache capacity in bytes.
    pub ram_cache_bytes: u64,
    /// Chunk size in bytes (used when a `build` workload must split a file).
    pub chunk_size: u32,
    /// Path to a pre-built image. Required for `inspect`, `verify`, `mount`,
    /// and all read workloads.
    pub image: Option<PathBuf>,
    /// Path to a source directory. Required for the `build` workload.
    pub data_dir: Option<PathBuf>,
}

impl Default for BenchConfig {
    fn default() -> Self {
        Self {
            iterations: 3,
            read_ahead: true,
            ram_cache_bytes: 256 * 1024 * 1024,
            chunk_size: swiftfs_core::DEFAULT_CHUNK_SIZE,
            image: None,
            data_dir: None,
        }
    }
}

/// Result of a single benchmark measurement.
#[derive(Debug, Clone)]
pub struct BenchmarkResult {
    /// Workload name.
    pub name: String,
    /// Preparation (build/inspect/verify/mount-load) time in milliseconds.
    pub mount_ms: u64,
    /// Mean time to first successful file access in milliseconds.
    pub ttfa_ms: f64,
    /// Total bytes fetched from the backend before the first useful operation.
    pub bytes_fetched: u64,
    /// Total wall-clock time of the measured phase in milliseconds.
    pub total_ms: u64,
    /// p50 read latency in milliseconds.
    pub p50_ms: f64,
    /// p99 read latency in milliseconds.
    pub p99_ms: f64,
    /// Peak resident set size in bytes (best effort, from `/proc/self/status`).
    pub peak_rss: u64,
    /// Fraction of reads served without touching the backend (0.0 - 1.0).
    pub cache_hit_rate: f64,
    /// Number of backend storage fetches actually performed.
    pub storage_requests: u64,
    /// Raw metrics map (sample counts, per-iteration detail, etc.).
    pub raw: BTreeMap<String, String>,
}

impl BenchmarkResult {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            mount_ms: 0,
            ttfa_ms: 0.0,
            bytes_fetched: 0,
            total_ms: 0,
            p50_ms: 0.0,
            p99_ms: 0.0,
            peak_rss: 0,
            cache_hit_rate: 0.0,
            storage_requests: 0,
            raw: BTreeMap::new(),
        }
    }
}

/// Peak RSS in bytes, read from `/proc/self/status` (`VmHWM`). Returns 0 if the
/// file or field is unavailable (e.g. non-Linux).
fn peak_rss_bytes() -> u64 {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return 0;
    };
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let kb: u64 = rest
                .split_whitespace()
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            return kb * 1024;
        }
    }
    0
}

/// Compute a percentile (nearest-rank) over a non-empty, unsorted sample slice.
fn percentile(samples: &mut [f64], pct: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let rank = (pct / 100.0 * samples.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(samples.len() - 1);
    samples[idx]
}

/// Build the fetcher the read path uses, sized per the benchmark config.
fn build_fetcher(image: &Path, config: &BenchConfig) -> Arc<ChunkFetcher> {
    let store: Arc<dyn ChunkStore> = Arc::new(LocalStore::for_image(image));
    let ram = Arc::new(RamCache::new(config.ram_cache_bytes));
    Arc::new(ChunkFetcher::new(
        store,
        ram,
        None,
        FetcherConfig::default(),
    ))
}

/// Time manifest load + index construction (the pre-FUSE work of `mount`).
fn bench_mount(config: &BenchConfig) -> anyhow::Result<BenchmarkResult> {
    let image = require_image(config)?;
    let mut result = BenchmarkResult::new("mount");
    let mut samples = Vec::with_capacity(config.iterations);

    for _ in 0..config.iterations.max(1) {
        let t0 = Instant::now();
        let manifest = load_manifest(image)?;
        let index = ManifestIndex::new(manifest)?;
        let elapsed = t0.elapsed();
        std::hint::black_box(index.manifest().nodes.len());
        samples.push(elapsed);
    }

    let total: Duration = samples.iter().sum();
    result.mount_ms = (total / samples.len() as u32).as_millis() as u64;
    result.total_ms = total.as_millis() as u64;
    result.mean_ms(&samples);
    result.raw.insert("nodes".into(), {
        let n = ManifestIndex::new(load_manifest(image)?)?
            .manifest()
            .nodes
            .len();
        n.to_string()
    });
    result.peak_rss = peak_rss_bytes();
    Ok(result)
}

/// Time an image build from a source directory.
fn bench_build(config: &BenchConfig) -> anyhow::Result<BenchmarkResult> {
    let source = config
        .data_dir
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("the `build` workload requires --data-dir <SOURCE DIR>"))?;
    let mut result = BenchmarkResult::new("build");

    // Build into a temporary sibling so repeated runs are clean.
    let out = std::env::temp_dir().join(format!("swiftfs-bench-build-{}", std::process::id()));
    if out.exists() {
        std::fs::remove_dir_all(&out).ok();
    }

    let t0 = Instant::now();
    let summary = build_image(
        source,
        &out,
        &BuildOptions {
            chunk_size: config.chunk_size,
        },
    )?;
    let elapsed = t0.elapsed();
    result.mount_ms = elapsed.as_millis() as u64;
    result.total_ms = elapsed.as_millis() as u64;
    result.bytes_fetched = summary.total_bytes;
    result
        .raw
        .insert("files".into(), summary.file_count.to_string());
    result
        .raw
        .insert("chunks".into(), summary.total_chunks.to_string());
    result
        .raw
        .insert("unique_chunks".into(), summary.unique_chunks.to_string());
    result.peak_rss = peak_rss_bytes();

    std::fs::remove_dir_all(&out).ok();
    Ok(result)
}

/// Time `inspect` or `verify` on a pre-built image.
fn bench_image_op(config: &BenchConfig, which: &str) -> anyhow::Result<BenchmarkResult> {
    let image = require_image(config)?;
    let mut result = BenchmarkResult::new(which);
    let mut samples = Vec::with_capacity(config.iterations);

    for _ in 0..config.iterations.max(1) {
        let t0 = Instant::now();
        match which {
            "inspect" => {
                let s = inspect_image(image)?;
                std::hint::black_box(s.chunk_count);
            }
            "verify" => {
                let s = verify_image(image)?;
                std::hint::black_box(s.scanned_objects);
            }
            _ => unreachable!(),
        }
        samples.push(t0.elapsed());
    }

    let total: Duration = samples.iter().sum();
    result.mount_ms = (total / samples.len() as u32).as_millis() as u64;
    result.total_ms = total.as_millis() as u64;
    result.mean_ms(&samples);
    result.peak_rss = peak_rss_bytes();
    Ok(result)
}

/// Perform real chunk reads over the local backend and collect latency samples.
///
/// `warm` controls whether the same bytes are read twice (populating then
/// relying on the cache). `prefetch` mirrors the daemon's read-ahead: after each
/// chunk read, sequential access is observed and following chunks are prefetched
/// on the runtime, exactly as the FUSE `read` callback does. Both paths read the
/// entire logical file chunk by chunk, in file order, which is what sequential
/// FUSE reads produce.
fn bench_read_named(
    config: &BenchConfig,
    warm: bool,
    prefetch: bool,
    name: &str,
) -> anyhow::Result<BenchmarkResult> {
    let image = require_image(config)?;
    let manifest = load_manifest(image)?;
    let index = ManifestIndex::new(manifest)?;
    let fetcher = build_fetcher(image, config);

    // Choose a target file: the largest file in the image.
    let target = index
        .manifest()
        .nodes
        .iter()
        .filter(|n| n.kind == swiftfs_format::NodeKind::File && n.size > 0)
        .max_by_key(|n| n.size)
        .ok_or_else(|| anyhow::anyhow!("image has no non-empty files to read"))?;

    let mut result = BenchmarkResult::new(name);
    let mut samples: Vec<f64> = Vec::new();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let handle = rt.handle().clone();

    let read_all =
        |fetcher: &Arc<ChunkFetcher>, samples: &mut Vec<f64>| -> anyhow::Result<Option<f64>> {
            let mut ttfa = None;
            let tracker = crate::read_ahead::ReadAhead::new();
            for chunk in &target.chunks {
                let location = ChunkLocation {
                    object: chunk.object.clone(),
                    offset: chunk.object_offset,
                    length: chunk.length,
                };
                let t0 = Instant::now();
                let bytes = handle.block_on(fetcher.get(&chunk.hash, &location))?;
                let elapsed = t0.elapsed();
                std::hint::black_box(bytes.len());
                if ttfa.is_none() {
                    ttfa = Some(elapsed.as_secs_f64() * 1000.0);
                }
                samples.push(elapsed.as_secs_f64() * 1000.0);

                // Mirror the daemon: on a sequential read, prefetch the next
                // chunks on the runtime without blocking this read.
                if prefetch && tracker.observe(0, chunk.offset, chunk.length) {
                    let end = chunk.offset + u64::from(chunk.length);
                    let next: Vec<(String, ChunkLocation)> = index
                        .chunks_after(target, end, crate::read_ahead::READ_AHEAD_CHUNKS)
                        .into_iter()
                        .map(|c| {
                            (
                                c.hash.clone(),
                                ChunkLocation {
                                    object: c.object.clone(),
                                    offset: c.object_offset,
                                    length: c.length,
                                },
                            )
                        })
                        .collect();
                    let f = Arc::clone(fetcher);
                    handle.spawn(async move {
                        for (hash, loc) in next {
                            let _ = f.get(&hash, &loc).await;
                        }
                    });
                }
            }
            Ok(ttfa)
        };

    // Warm-up (uncounted) pass, then measured passes.
    let warmup_start = Instant::now();
    let _ = read_all(&fetcher, &mut Vec::new())?;
    result.raw.insert(
        "warmup_ms".into(),
        warmup_start.elapsed().as_millis().to_string(),
    );

    // Snapshot counters after warm-up so measured-pass figures exclude it.
    let m = fetcher.metrics();
    let req_before = m.requests.get();
    let backend_before = m.backend_fetches.get();
    let ram_before = m.ram_hits.get();

    // For a cold read, clear the RAM cache before each measured pass so every
    // measured pass genuinely misses. For a warm read, leave the cache populated
    // by the warm-up pass so measured passes hit it.
    let mut ttfa = None;
    let t0 = Instant::now();
    for _ in 0..config.iterations.max(1) {
        if !warm {
            fetcher.ram().clear();
        }
        let pass_ttfa = read_all(&fetcher, &mut samples)?;
        if ttfa.is_none() {
            ttfa = pass_ttfa;
        }
    }
    result.total_ms = t0.elapsed().as_millis() as u64;
    result.ttfa_ms = ttfa.unwrap_or(0.0);

    let requests = m.requests.get() - req_before;
    let backend = m.backend_fetches.get() - backend_before;
    let ram_hits = m.ram_hits.get() - ram_before;
    result.storage_requests = backend;
    result.bytes_fetched = backend.saturating_mul(u64::from(config.chunk_size));
    result.cache_hit_rate = if requests == 0 {
        0.0
    } else {
        (requests.saturating_sub(backend)) as f64 / requests as f64
    };

    result.p50_ms = percentile(&mut samples, 50.0);
    result.p99_ms = percentile(&mut samples, 99.0);
    result
        .raw
        .insert("samples".into(), samples.len().to_string());
    result.raw.insert("ram_hits".into(), ram_hits.to_string());
    result
        .raw
        .insert("backend_fetches".into(), backend.to_string());
    result.raw.insert("prefetch".into(), prefetch.to_string());
    result.peak_rss = peak_rss_bytes();

    drop(rt);
    Ok(result)
}

/// Compare cold vs warm cache for the same file.
fn bench_compare(config: &BenchConfig) -> anyhow::Result<BenchmarkResult> {
    let cold = bench_read_named(config, false, false, "read")?;
    let warm = bench_read_named(config, true, false, "warm_read")?;
    let mut result = BenchmarkResult::new("compare");
    result.p50_ms = cold.p50_ms;
    result.p99_ms = cold.p99_ms;
    result.total_ms = cold.total_ms + warm.total_ms;
    result.bytes_fetched = cold.bytes_fetched;
    result.storage_requests = cold.storage_requests;
    result.cache_hit_rate = warm.cache_hit_rate;
    result
        .raw
        .insert("cold_p50_ms".into(), format!("{:.4}", cold.p50_ms));
    result
        .raw
        .insert("warm_p50_ms".into(), format!("{:.4}", warm.p50_ms));
    result
        .raw
        .insert("cold_p99_ms".into(), format!("{:.4}", cold.p99_ms));
    result
        .raw
        .insert("warm_p99_ms".into(), format!("{:.4}", warm.p99_ms));
    result
        .raw
        .insert("cold_backend".into(), cold.storage_requests.to_string());
    result
        .raw
        .insert("warm_backend".into(), warm.storage_requests.to_string());
    result.peak_rss = peak_rss_bytes();
    Ok(result)
}

fn require_image(config: &BenchConfig) -> anyhow::Result<&Path> {
    config
        .image
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("this workload requires --image <IMAGE DIR>"))
}

impl BenchmarkResult {
    /// Fill `p50_ms`/`p99_ms`/`ttfa_ms` from a set of duration samples.
    fn mean_ms(&mut self, samples: &[Duration]) {
        if samples.is_empty() {
            return;
        }
        let mut ms: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
        self.p50_ms = percentile(&mut ms.clone(), 50.0);
        self.p99_ms = percentile(&mut ms, 99.0);
        self.ttfa_ms = samples[0].as_secs_f64() * 1000.0;
    }
}

/// Run a single benchmark workload, performing real measurements.
pub fn run_benchmark(config: &BenchConfig, workload: &str) -> anyhow::Result<BenchmarkResult> {
    match workload {
        "build" => bench_build(config),
        "inspect" => bench_image_op(config, "inspect"),
        "verify" => bench_image_op(config, "verify"),
        "mount" => bench_mount(config),
        "read" => bench_read_named(config, false, false, "read"),
        "warm_read" => bench_read_named(config, true, false, "warm_read"),
        "compare" => bench_compare(config),
        "read-ahead" => bench_read_named(config, false, config.read_ahead, "read-ahead"),
        "io" => bench_read_named(config, false, false, "io"),
        other => anyhow::bail!(
            "unknown benchmark workload {other:?}; supported: build, inspect, verify, \
             mount, read, warm_read, compare, read-ahead, io"
        ),
    }
}

/// Run every workload in order and print a summary table.
///
/// For read workloads an `image` must be configured. If no image is given, the
/// build-only workloads are run and the rest are skipped with a notice.
pub fn run_full_suite(config: &BenchConfig) -> anyhow::Result<()> {
    println!("SwiftFS benchmark suite");
    println!("  iterations:      {}", config.iterations);
    println!("  read_ahead:      {}", config.read_ahead);
    println!("  ram_cache_bytes: {}", config.ram_cache_bytes);
    println!("  chunk_size:      {}", config.chunk_size);
    match &config.image {
        Some(p) => println!("  image:           {}", p.display()),
        None => println!("  image:           (none; read workloads will be skipped)"),
    }
    match &config.data_dir {
        Some(p) => println!("  data_dir:        {}", p.display()),
        None => println!("  data_dir:        (none; build will be skipped)"),
    }
    println!();

    let workloads: &[&str] = &[
        "build",
        "inspect",
        "verify",
        "mount",
        "read",
        "warm_read",
        "compare",
        "read-ahead",
    ];

    let mut rows: Vec<BenchmarkResult> = Vec::new();
    for &w in workloads {
        if (w == "build") && config.data_dir.is_none() {
            println!("skip {w}: set --data-dir to benchmark building");
            continue;
        }
        if w != "build" && config.image.is_none() {
            println!("skip {w}: set --image to benchmark image operations and reads");
            continue;
        }
        match run_benchmark(config, w) {
            Ok(r) => {
                println!(
                    "{:<10} p50={:>8.3}ms p99={:>8.3}ms ttfa={:>8.3}ms total={:>6}ms backend={:>5} hit={:>5.1}%",
                    r.name,
                    r.p50_ms,
                    r.p99_ms,
                    r.ttfa_ms,
                    r.total_ms,
                    r.storage_requests,
                    r.cache_hit_rate * 100.0,
                );
                rows.push(r);
            }
            Err(e) => println!("{w:<10} failed: {e}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_nearest_rank() {
        let v = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];
        assert_eq!(percentile(&mut v.clone(), 50.0), 5.0);
        assert_eq!(percentile(&mut v.clone(), 99.0), 10.0);
        assert_eq!(percentile(&mut [42.0], 50.0), 42.0);
        assert_eq!(percentile(&mut [], 50.0), 0.0);
    }

    #[test]
    fn unknown_workload_errors() {
        let cfg = BenchConfig::default();
        assert!(run_benchmark(&cfg, "nope").is_err());
    }

    #[test]
    fn read_requires_image() {
        let cfg = BenchConfig::default();
        assert!(run_benchmark(&cfg, "read").is_err());
    }
}
