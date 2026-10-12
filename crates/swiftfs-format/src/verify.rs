//! Image inspection and integrity verification.

use std::fs;
use std::path::Path;

use swiftfs_core::{Result, SwiftfsError};
use tracing::{info, warn};

use crate::builder::ImageLayout;
use crate::manifest::{hash_bytes, Manifest, NodeKind};

/// Load a manifest from an image directory or a direct manifest path.
pub fn load_manifest(image: &Path) -> Result<Manifest> {
    let manifest_path = if image.is_file() {
        image.to_path_buf()
    } else {
        image.join(crate::builder::MANIFEST_FILE)
    };
    let bytes = fs::read(&manifest_path).map_err(|e| {
        SwiftfsError::InvalidImage(format!("cannot read manifest {:?}: {e}", manifest_path))
    })?;
    Manifest::from_json(&bytes)
}

/// Summary of an image, as produced by `inspect`.
#[derive(Debug, Clone)]
pub struct InspectSummary {
    pub format_version: u32,
    pub chunk_size: u32,
    pub hash_algorithm: String,
    pub file_count: u64,
    pub dir_count: u64,
    pub total_bytes: u64,
    pub chunk_count: u64,
    pub unique_chunk_count: u64,
    pub stored_chunk_bytes: u64,
    pub manifest_bytes: u64,
}

/// Inspect an image without reading chunk contents.
pub fn inspect_image(image: &Path) -> Result<InspectSummary> {
    let manifest = load_manifest(image)?;
    let layout = ImageLayout::new(image);

    let mut chunk_count = 0u64;
    let mut unique: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut stored_chunk_bytes = 0u64;

    for node in &manifest.nodes {
        if node.kind != NodeKind::File {
            continue;
        }
        chunk_count += node.chunks.len() as u64;
        for chunk in &node.chunks {
            if unique.insert(chunk.object.as_str()) {
                stored_chunk_bytes += u64::from(chunk.length);
            }
        }
    }

    let manifest_bytes = fs::metadata(layout.manifest_path())
        .map(|m| m.len())
        .unwrap_or(0);

    Ok(InspectSummary {
        format_version: manifest.format_version,
        chunk_size: manifest.chunk_size,
        hash_algorithm: manifest.hash_algorithm.clone(),
        file_count: manifest.source.file_count,
        dir_count: manifest.source.dir_count,
        total_bytes: manifest.source.total_bytes,
        chunk_count,
        unique_chunk_count: unique.len() as u64,
        stored_chunk_bytes,
        manifest_bytes,
    })
}

/// Result of verifying an image.
#[derive(Debug, Clone)]
pub struct VerifySummary {
    /// Chunks whose hash could not be recomputed.
    pub scanned_chunks: u64,
    /// Unique objects examined.
    pub scanned_objects: u64,
}

/// Verify that every referenced chunk object exists and matches its hash.
///
/// Reads each unique object once. Returns an error on the first mismatch,
/// missing object, or truncated/short object.
pub fn verify_image(image: &Path) -> Result<VerifySummary> {
    let manifest = load_manifest(image)?;
    let layout = ImageLayout::new(image);

    // object id -> expected hash. Reject inconsistent manifests where the same
    // object id is claimed to hold different content.
    let mut objects: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut scanned_chunks = 0u64;

    for node in &manifest.nodes {
        for chunk in &node.chunks {
            scanned_chunks += 1;
            match objects.get(&chunk.object) {
                Some(existing) if existing != &chunk.hash => {
                    return Err(SwiftfsError::InvalidImage(format!(
                        "object {:?} referenced with conflicting hashes",
                        chunk.object
                    )));
                }
                _ => {
                    objects.insert(chunk.object.clone(), chunk.hash.clone());
                }
            }
        }
    }

    let mut scanned_objects = 0u64;
    for (object, expected_hash) in &objects {
        let path = layout.object_path(object);
        let data = fs::read(&path).map_err(|e| {
            SwiftfsError::ChunkNotFound(format!("object {:?} unreadable: {e}", path))
        })?;
        // Object may contain a chunk at object_offset != 0 in future formats;
        // in format v1 chunks occupy the whole object.
        let actual = hash_bytes(&data);
        if &actual != expected_hash {
            return Err(SwiftfsError::IntegrityFailure(format!(
                "object {:?}: expected {}, got {}",
                object, expected_hash, actual
            )));
        }
        scanned_objects += 1;
    }

    info!(scanned_chunks, scanned_objects, "image verified");
    Ok(VerifySummary {
        scanned_chunks,
        scanned_objects,
    })
}

/// Read the bytes for a logical byte range of a node, reassembling from chunk
/// objects. Used by tests and the benchmark harness to prove byte-for-byte
/// correctness; the FUSE layer performs the same mapping via the cache.
pub fn read_node_range(
    image: &Path,
    node: &crate::manifest::Node,
    offset: u64,
    length: u64,
) -> Result<Vec<u8>> {
    use crate::manifest::NodeKind;

    if node.kind != NodeKind::File {
        return Err(SwiftfsError::InvalidImage(format!(
            "node {:?} is not a file",
            node.path
        )));
    }
    let end = offset
        .checked_add(length)
        .ok_or_else(|| SwiftfsError::InvalidImage("read range overflow".into()))?;
    if end > node.size {
        return Err(SwiftfsError::InvalidImage(format!(
            "read range {}..{} exceeds size {}",
            offset, end, node.size
        )));
    }

    let mut out = Vec::with_capacity(length as usize);
    for chunk in &node.chunks {
        let chunk_start = chunk.offset;
        let chunk_end = chunk.offset + u64::from(chunk.length);
        if chunk_end <= offset || chunk_start >= end {
            continue;
        }
        let data = read_chunk_bytes(image, &chunk.object, chunk.object_offset, chunk.length)?;
        let local_start = offset.saturating_sub(chunk_start) as usize;
        let local_end = (end.min(chunk_end) - chunk_start) as usize;
        out.extend_from_slice(&data[local_start..local_end]);
    }
    if out.len() as u64 != length {
        return Err(SwiftfsError::InvalidImage(format!(
            "reassembled {} bytes, expected {}",
            out.len(),
            length
        )));
    }
    Ok(out)
}

/// Compare a chunk's bytes against the source file range, used by tests and the
/// benchmark harness to prove byte-for-byte correctness.
pub fn read_chunk_bytes(image: &Path, object: &str, offset: u64, length: u32) -> Result<Vec<u8>> {
    let layout = ImageLayout::new(image);
    let data = fs::read(layout.object_path(object))?;
    let start = offset as usize;
    let end = start
        .checked_add(length as usize)
        .ok_or_else(|| SwiftfsError::InvalidImage("chunk range overflow".into()))?;
    if end > data.len() {
        warn!("chunk range exceeds object length");
        return Err(SwiftfsError::IntegrityFailure(format!(
            "chunk range {}..{} exceeds object length {}",
            start,
            end,
            data.len()
        )));
    }
    Ok(data[start..end].to_vec())
}
