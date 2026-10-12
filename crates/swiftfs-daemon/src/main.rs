//! `swiftfs` command-line entry point.
//!
//! Phase 1 wires up the CLI shell. Subcommands gain real behavior as the
//! corresponding phases land (build/verify in Phase 2, mount in Phase 3+).

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "swiftfs",
    version,
    about = "High-performance lazy-loading container filesystem",
    long_about = "SwiftFS exposes a read-only image tree through FUSE while fetching \
                  file data only when accessed. See docs/ for architecture and usage."
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
        Command::Build { .. } => anyhow::bail!("`build` is implemented in Phase 2"),
        Command::Inspect { .. } => anyhow::bail!("`inspect` is implemented in Phase 2"),
        Command::Verify { .. } => anyhow::bail!("`verify` is implemented in Phase 2"),
        Command::Mount { .. } => anyhow::bail!("`mount` is implemented in Phase 3"),
        Command::Unmount { .. } => anyhow::bail!("`unmount` is implemented in Phase 3"),
        Command::Bench { workload } => {
            anyhow::bail!("`bench {workload}` is implemented in Phase 6")
        }
    }
}
