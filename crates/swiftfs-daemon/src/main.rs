//! `swiftfs` command-line entry point.
//!
//! Phase 2 implements `build`, `inspect`, and `verify`. `mount`/`unmount`
//! arrive in Phase 3 and `bench` in Phase 6.

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
        swiftfs mount ./myimage /mnt/swiftfs"
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
    /// Run or invoke supported benchmark workloads.
    Bench {
        /// Benchmark workload name.
        #[arg(default_value = "help")]
        workload: String,
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
        Command::Mount { .. } => anyhow::bail!("`mount` is implemented in Phase 3"),
        Command::Unmount { .. } => anyhow::bail!("`unmount` is implemented in Phase 3"),
        Command::Bench { workload } => {
            anyhow::bail!("`bench {workload}` is implemented in Phase 6")
        }
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
