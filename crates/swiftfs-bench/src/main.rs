//! Benchmark harness entry point.
//!
//! A thin front end over [`swiftfs_daemon::benchmark`], which performs the real
//! measurements. `swiftfs bench` provides the same functionality inside the main
//! CLI; this binary exists for harnesses that prefer a dedicated executable.

use clap::Parser;
use swiftfs_daemon::{run_benchmark, run_full_suite, BenchConfig};

#[derive(Debug, Parser)]
#[command(
    name = "swiftfs-bench",
    version,
    about = "Benchmark harness for SwiftFS image operations and read latency",
    after_help = "Examples:\n  \
        swiftfs-bench --image ./myimage --data-dir ./myroot\n  \
        swiftfs-bench --workload read --image ./myimage\n  \
        swiftfs-bench --workload build --data-dir ./myroot"
)]
struct Cli {
    /// Workload: build, inspect, verify, mount, read, warm_read, compare,
    /// read-ahead, io, or "full" (default) for the whole suite.
    #[arg(long, default_value = "full")]
    workload: String,
    /// Path to a pre-built image (required for all workloads except build).
    #[arg(long)]
    image: Option<std::path::PathBuf>,
    /// Path to a source directory (required for the build workload).
    #[arg(long)]
    data_dir: Option<std::path::PathBuf>,
    /// Measured iterations per workload.
    #[arg(long, default_value_t = 3)]
    iterations: usize,
    /// Chunk size in bytes (build workload).
    #[arg(long, default_value_t = swiftfs_core::DEFAULT_CHUNK_SIZE)]
    chunk_size: u32,
    /// RAM cache capacity in bytes.
    #[arg(long, default_value_t = 256 * 1024 * 1024)]
    ram_cache_bytes: u64,
    /// Disable read-ahead.
    #[arg(long)]
    no_read_ahead: bool,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();
    let config = BenchConfig {
        iterations: cli.iterations,
        read_ahead: !cli.no_read_ahead,
        ram_cache_bytes: cli.ram_cache_bytes,
        chunk_size: cli.chunk_size,
        image: cli.image,
        data_dir: cli.data_dir,
    };

    if cli.workload == "full" || cli.workload == "help" {
        return run_full_suite(&config);
    }

    let r = run_benchmark(&config, &cli.workload)?;
    println!("workload:        {}", r.name);
    println!("p50 latency:     {:.3} ms", r.p50_ms);
    println!("p99 latency:     {:.3} ms", r.p99_ms);
    println!("ttfa:            {:.3} ms", r.ttfa_ms);
    println!("prep/total:      {} ms / {} ms", r.mount_ms, r.total_ms);
    println!("cache hit rate:  {:.1}%", r.cache_hit_rate * 100.0);
    println!("storage fetches: {}", r.storage_requests);
    println!("bytes fetched:   {}", r.bytes_fetched);
    println!("peak rss:        {} KiB", r.peak_rss / 1024);
    for (k, v) in &r.raw {
        println!("  {k}: {v}");
    }
    Ok(())
}
