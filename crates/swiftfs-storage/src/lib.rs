//! Chunk storage backends.
//!
//! Defines a backend-agnostic trait so the filesystem can fetch chunk bytes
//! without depending on a specific transport. Implementations (local
//! directory, HTTP byte-range) arrive in Phase 4.

#![forbid(unsafe_code)]

/// Identifies a storage object and the byte range within it that holds a chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkLocation {
    /// Backend-specific object key, e.g. a content-addressed chunk id.
    pub object: String,
    /// Byte offset of the chunk within the object.
    pub offset: u64,
    /// Length of the chunk in bytes.
    pub length: u32,
}
