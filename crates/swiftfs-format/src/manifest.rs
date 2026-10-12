//! Versioned manifest schema for a SwiftFS image.
//!
//! The manifest is immutable once written. Everything here is deserialized from
//! potentially untrusted input, so structs are plain data and all invariants are
//! enforced by [`Manifest::validate`] rather than by construction.

use serde::{Deserialize, Serialize};
use swiftfs_core::{Result, SwiftfsError, DEFAULT_CHUNK_SIZE, ROOT_INODE};

/// Current on-disk format version.
pub const FORMAT_VERSION: u32 = 1;

/// Hash algorithm identifier used by this format version.
pub const HASH_ALGORITHM: &str = "blake3";

/// Maximum permitted chunk size (64 MiB) to bound validation-time allocations.
pub const MAX_CHUNK_SIZE: u32 = 64 * 1024 * 1024;

/// Kind of a filesystem node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeKind {
    Dir,
    File,
}

/// A single independently addressable chunk of a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    /// Byte offset of this chunk within the logical file.
    pub offset: u64,
    /// Length of the chunk in bytes.
    pub length: u32,
    /// BLAKE3 hash of the (uncompressed) chunk bytes, hex-encoded.
    pub hash: String,
    /// Content-addressed object id this chunk is stored in.
    pub object: String,
    /// Byte offset of the chunk within the storage object.
    pub object_offset: u64,
}

/// A file or directory in the image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    /// Stable, non-zero inode number. Root is [`ROOT_INODE`].
    pub inode: u64,
    /// Absolute, normalized, non-escaping path.
    pub path: String,
    /// Node kind.
    pub kind: NodeKind,
    /// POSIX mode bits.
    pub mode: u32,
    /// Modification time, seconds since the Unix epoch.
    pub mtime: i64,
    /// File size in bytes (0 for directories).
    pub size: u64,
    /// Ordered chunks (empty for directories and empty files).
    #[serde(default)]
    pub chunks: Vec<Chunk>,
}

/// Aggregate statistics about the source image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceStats {
    /// Total logical bytes across all files.
    pub total_bytes: u64,
    /// Number of file nodes.
    pub file_count: u64,
    /// Number of directory nodes (including root).
    pub dir_count: u64,
}

/// The complete image manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Format version; must equal [`FORMAT_VERSION`].
    pub format_version: u32,
    /// Chunk size used when the image was built.
    pub chunk_size: u32,
    /// Hash algorithm identifier; must equal [`HASH_ALGORITHM`].
    pub hash_algorithm: String,
    /// Aggregate source statistics.
    pub source: SourceStats,
    /// All nodes, in no particular required order.
    pub nodes: Vec<Node>,
}

impl Manifest {
    /// Validate structural invariants. Rejects malformed or hostile input.
    pub fn validate(&self) -> Result<()> {
        if self.format_version != FORMAT_VERSION {
            return Err(SwiftfsError::InvalidImage(format!(
                "unsupported format_version {} (expected {})",
                self.format_version, FORMAT_VERSION
            )));
        }
        if self.hash_algorithm != HASH_ALGORITHM {
            return Err(SwiftfsError::InvalidImage(format!(
                "unsupported hash_algorithm {:?}",
                self.hash_algorithm
            )));
        }
        if self.chunk_size == 0 || self.chunk_size > MAX_CHUNK_SIZE {
            return Err(SwiftfsError::InvalidImage(format!(
                "chunk_size {} out of range (1..={})",
                self.chunk_size, MAX_CHUNK_SIZE
            )));
        }
        if self.nodes.is_empty() {
            return Err(SwiftfsError::InvalidImage("no nodes".into()));
        }

        let mut inodes: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let mut paths: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut saw_root = false;

        for node in &self.nodes {
            if node.inode == 0 {
                return Err(SwiftfsError::InvalidImage("inode 0 is reserved".into()));
            }
            if !inodes.insert(node.inode) {
                return Err(SwiftfsError::InvalidImage(format!(
                    "duplicate inode {}",
                    node.inode
                )));
            }
            validate_path(&node.path)?;
            if !paths.insert(node.path.as_str()) {
                return Err(SwiftfsError::InvalidImage(format!(
                    "duplicate path {:?}",
                    node.path
                )));
            }
            if node.inode == ROOT_INODE {
                if node.kind != NodeKind::Dir || node.path != "/" {
                    return Err(SwiftfsError::InvalidImage(
                        "root inode must be a directory at \"/\"".into(),
                    ));
                }
                saw_root = true;
            }
            validate_node_chunks(self.chunk_size, node)?;
        }

        if !saw_root {
            return Err(SwiftfsError::InvalidImage(format!(
                "missing root inode {ROOT_INODE}"
            )));
        }
        Ok(())
    }

    /// Serialize to pretty JSON, used for writing the manifest to disk.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self)
            .map_err(|e| SwiftfsError::InvalidImage(format!("manifest serialization failed: {e}")))
    }

    /// Parse and validate a manifest from JSON bytes.
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        let manifest: Manifest = serde_json::from_slice(bytes)
            .map_err(|e| SwiftfsError::InvalidImage(format!("manifest parse failed: {e}")))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Look up a node by path.
    pub fn node_by_path(&self, path: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.path == path)
    }

    /// Look up a node by inode.
    pub fn node_by_inode(&self, inode: u64) -> Option<&Node> {
        self.nodes.iter().find(|n| n.inode == inode)
    }
}

/// Validate a manifest path: absolute, normalized, and non-escaping.
pub fn validate_path(path: &str) -> Result<()> {
    if !path.starts_with('/') {
        return Err(SwiftfsError::PathRejected(format!(
            "path must be absolute: {path:?}"
        )));
    }
    if path.contains('\0') {
        return Err(SwiftfsError::PathRejected(format!(
            "path contains NUL: {path:?}"
        )));
    }
    if path == "/" {
        return Ok(());
    }
    if path.ends_with('/') {
        return Err(SwiftfsError::PathRejected(format!(
            "path has trailing slash: {path:?}"
        )));
    }
    for component in path.split('/') {
        match component {
            "" => continue,
            "." => {
                return Err(SwiftfsError::PathRejected(format!(
                    "path contains '.' component: {path:?}"
                )))
            }
            ".." => {
                return Err(SwiftfsError::PathRejected(format!(
                    "path traversal not allowed: {path:?}"
                )))
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_node_chunks(chunk_size: u32, node: &Node) -> Result<()> {
    match node.kind {
        NodeKind::Dir => {
            if !node.chunks.is_empty() {
                return Err(SwiftfsError::InvalidImage(format!(
                    "directory {:?} has chunks",
                    node.path
                )));
            }
            if node.size != 0 {
                return Err(SwiftfsError::InvalidImage(format!(
                    "directory {:?} has non-zero size",
                    node.path
                )));
            }
            Ok(())
        }
        NodeKind::File => {
            if node.size == 0 {
                if !node.chunks.is_empty() {
                    return Err(SwiftfsError::InvalidImage(format!(
                        "empty file {:?} has chunks",
                        node.path
                    )));
                }
                return Ok(());
            }

            let mut expected_offset: u64 = 0;
            for (i, chunk) in node.chunks.iter().enumerate() {
                if chunk.length == 0 {
                    return Err(SwiftfsError::InvalidImage(format!(
                        "file {:?} chunk {i} has zero length",
                        node.path
                    )));
                }
                if chunk.length > chunk_size {
                    return Err(SwiftfsError::InvalidImage(format!(
                        "file {:?} chunk {i} length {} exceeds chunk_size {}",
                        node.path, chunk.length, chunk_size
                    )));
                }
                if chunk.offset != expected_offset {
                    return Err(SwiftfsError::InvalidImage(format!(
                        "file {:?} chunk {i} offset {} does not follow previous chunk (expected {})",
                        node.path, chunk.offset, expected_offset
                    )));
                }
                if chunk.hash.len() != 64 || !chunk.hash.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Err(SwiftfsError::InvalidImage(format!(
                        "file {:?} chunk {i} has malformed blake3 hash",
                        node.path
                    )));
                }
                if chunk.object.is_empty() {
                    return Err(SwiftfsError::InvalidImage(format!(
                        "file {:?} chunk {i} has empty object id",
                        node.path
                    )));
                }
                expected_offset = expected_offset
                    .checked_add(u64::from(chunk.length))
                    .ok_or_else(|| {
                        SwiftfsError::InvalidImage(format!(
                            "file {:?} chunk {i} overflows offset arithmetic",
                            node.path
                        ))
                    })?;
            }

            if expected_offset != node.size {
                return Err(SwiftfsError::InvalidImage(format!(
                    "file {:?} chunks cover {} bytes but size is {}",
                    node.path, expected_offset, node.size
                )));
            }
            Ok(())
        }
    }
}

/// Compute the BLAKE3 hash of `data` as a lowercase hex string.
pub fn hash_bytes(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
}

/// Default chunk size, re-exported for convenience.
pub const DEFAULT_CHUNK: u32 = DEFAULT_CHUNK_SIZE;
