//! `swiftfs` command-line entry point.
//!
//! Implements `build`, `inspect`, `verify`, `mount`, `unmount`, and `bench`.

use clap::{Parser, Subcommand};
use swiftfs_format::{build_image, inspect_image, verify_image, BuildOptions};

#[derive(Debug, Parser)]
#[command(
    name = "swiftfs",
    version,
    about = "High-performance lazy-loading container filesystem",
    long_about = "SwiftFS exposes a read-only image tree through FUSE while fetching \
                  file data only when accessed. See docs/ for architecture and usage.",
    after_help = "Examples:\n  \
        swiftfs build ./myroot ./myimage\n  \
        swiftfs build ./myroot ./myimage --chunk-size 262144\n  \
        swiftfs inspect ./myimage\n  \
        swiftfs verify ./myimage\n  \
        swiftfs mount ./myimage /mnt/swiftfs\n  \
        swiftfs bench full --image ./myimage --data-dir ./myroot"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Build a SwiftFS image from a source directory.
    Build {
        /// Source directory to package.
        source: std::path::PathBuf,
        /// Destination directory for the manifest and chunk objects.
        output: std::path::PathBuf,
        /// Chunk size in bytes.
        #[arg(long, default_value_t = swiftfs_core::DEFAULT_CHUNK_SIZE)]
        chunk_size: u32,
    },
    /// Inspect image metadata and size.
    Inspect {
        /// Path to a SwiftFS image directory or manifest.
        image: std::path::PathBuf,
    },
    /// Mount an image at a target path.
    Mount {
        /// Path to a SwiftFS image.
        image: std::path::PathBuf,
        /// Mountpoint.
        target: std::path::PathBuf,
        /// Allow other users to access the mount.
        #[arg(long)]
        allow_other: bool,
        /// Number of FUSE worker threads (Linux only).
        #[arg(long, default_value_t = 4)]
        threads: usize,
    },
    /// Cleanly unmount a mounted SwiftFS filesystem.
    Unmount {
        /// Mountpoint.
        target: std::path::PathBuf,
    },
    /// Verify manifest and chunk integrity.
    Verify {
        /// Path to a SwiftFS image.
        image: std::path::PathBuf,
    },
    /// Run benchmark workloads with real measurements.
    Bench {
        /// Workload: build, inspect, verify, mount, read, warm_read, compare,
        /// read-ahead, io, or "full" for the whole suite.
        #[arg(default_value = "full")]
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
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Build {
            source,
            output,
            chunk_size,
        } => command_build(source, output, chunk_size),
        Command::Inspect { image } => command_inspect(image),
        Command::Verify { image } => command_verify(image),
        Command::Mount {
            image,
            target,
            allow_other,
            threads,
        } => command_mount(image, target, allow_other, threads),
        Command::Unmount { target } => command_unmount(target),
        Command::Bench {
            workload,
            image,
            data_dir,
            iterations,
            chunk_size,
            ram_cache_bytes,
            no_read_ahead,
        } => command_bench(
            workload,
            image,
            data_dir,
            iterations,
            chunk_size,
            ram_cache_bytes,
            !no_read_ahead,
        ),
    }
}

fn command_build(
    source: std::path::PathBuf,
    output: std::path::PathBuf,
    chunk_size: u32,
) -> anyhow::Result<()> {
    let options = BuildOptions { chunk_size };
    let summary = build_image(&source, &output, &options)?;
    println!("Built image at {}", output.display());
    println!("  files:          {}", summary.file_count);
    println!("  directories:    {}", summary.dir_count);
    println!("  logical bytes:  {}", summary.total_bytes);
    println!("  unique chunks:  {}", summary.unique_chunks);
    println!("  total chunks:   {}", summary.total_chunks);
    Ok(())
}

fn command_inspect(image: std::path::PathBuf) -> anyhow::Result<()> {
    let s = inspect_image(&image)?;
    println!("Image: {}", image.display());
    println!("  format version: {:>14}", s.format_version);
    println!("  hash algorithm: {:>14}", s.hash_algorithm);
    println!("  chunk size:     {:>14}", s.chunk_size);
    println!("  files:          {:>14}", s.file_count);
    println!("  directories:    {:>14}", s.dir_count);
    println!("  logical bytes:  {:>14}", s.total_bytes);
    println!("  chunks:         {:>14}", s.chunk_count);
    println!("  unique chunks:  {:>14}", s.unique_chunk_count);
    println!("  stored bytes:   {:>14}", s.stored_chunk_bytes);
    println!("  manifest bytes: {:>14}", s.manifest_bytes);
    Ok(())
}

fn command_verify(image: std::path::PathBuf) -> anyhow::Result<()> {
    let s = verify_image(&image)?;
    println!(
        "OK: {} objects verified ({} chunk references)",
        s.scanned_objects, s.scanned_chunks
    );
    Ok(())
}

fn command_mount(
    image: std::path::PathBuf,
    target: std::path::PathBuf,
    allow_other: bool,
    threads: usize,
) -> anyhow::Result<()> {
    let mut config = swiftfs_daemon::MountConfig::new(image, target);
    config.allow_other = allow_other;
    config.n_threads = threads;
    swiftfs_daemon::mount_image(&config)?;
    Ok(())
}

fn command_unmount(target: std::path::PathBuf) -> anyhow::Result<()> {
    let target_str = target
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("mountpoint path is not valid UTF-8"))?;

    for cmd in ["fusermount3", "fusermount"] {
        match std::process::Command::new(cmd)
            .arg("-u")
            .arg(target_str)
            .status()
        {
            Ok(status) if status.success() => {
                println!("Unmounted {target_str}");
                return Ok(());
            }
            Ok(status) => anyhow::bail!("{cmd} -u {target_str} failed with {status}"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        }
    }

    match std::process::Command::new("umount")
        .arg(target_str)
        .status()
    {
        Ok(status) if status.success() => {
            println!("Unmounted {target_str}");
            Ok(())
        }
        Ok(status) => anyhow::bail!("umount {target_str} failed with {status}"),
        Err(e) => anyhow::bail!("no unmount tool available (fusermount3/umount): {e}"),
    }
}

#[allow(clippy::too_many_arguments)]
fn command_bench(
    workload: String,
    image: Option<std::path::PathBuf>,
    data_dir: Option<std::path::PathBuf>,
    iterations: usize,
    chunk_size: u32,
    ram_cache_bytes: u64,
    read_ahead: bool,
) -> anyhow::Result<()> {
    let config = swiftfs_daemon::BenchConfig {
        iterations,
        read_ahead,
        ram_cache_bytes,
        chunk_size,
        image,
        data_dir,
    };

    if workload == "full" || workload == "help" {
        return swiftfs_daemon::run_full_suite(&config);
    }

    let result = swiftfs_daemon::run_benchmark(&config, &workload)?;
    println!("workload:        {}", result.name);
    println!("p50 latency:     {:.3} ms", result.p50_ms);
    println!("p99 latency:     {:.3} ms", result.p99_ms);
    println!("ttfa:            {:.3} ms", result.ttfa_ms);
    println!(
        "prep/total:      {} ms / {} ms",
        result.mount_ms, result.total_ms
    );
    println!("cache hit rate:  {:.1}%", result.cache_hit_rate * 100.0);
    println!("storage fetches: {}", result.storage_requests);
    println!("bytes fetched:   {}", result.bytes_fetched);
    println!("peak rss:        {} KiB", result.peak_rss / 1024);
    if !result.raw.is_empty() {
        println!("raw:");
        for (k, v) in &result.raw {
            println!("  {k}: {v}");
        }
    }
    Ok(())
}
