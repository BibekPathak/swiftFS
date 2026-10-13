//! Core shared types and errors for SwiftFS.
//!
//! This crate intentionally holds only primitives that multiple other crates
//! depend on, to avoid cyclic dependencies between format, storage, cache, and
//! the FUSE daemon.

/// Size of a SwiftFS chunk in bytes unless overridden at build time.
pub const DEFAULT_CHUNK_SIZE: u32 = 1024 * 1024;

pub mod metrics;

/// Stable, in-memory inode number assigned to the filesystem root.
pub const ROOT_INODE: u64 = 1;

/// Errors that can occur across SwiftFS components.
#[derive(Debug, thiserror::Error)]
pub enum SwiftfsError {
    #[error("invalid image: {0}")]
    InvalidImage(String),

    #[error("path rejected: {0}")]
    PathRejected(String),

    #[error("unsupported file type: {0}")]
    UnsupportedFileType(String),

    #[error("chunk not found: {0}")]
    ChunkNotFound(String),

    #[error("integrity check failed for chunk {0}")]
    IntegrityFailure(String),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Result alias used throughout SwiftFS.
pub type Result<T> = std::result::Result<T, SwiftfsError>;
