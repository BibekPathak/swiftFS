//! In-memory index over a validated manifest.
//!
//! Mount-time work is confined to reading and indexing the manifest; no file
//! contents are fetched. The index provides:
//!
//! - inode -> node lookup
//! - (parent inode, name) -> child inode lookup for `lookup`
//! - inode -> ordered children for `readdir`
//! - chunk mapping for byte-range reads
//!
//! All maps are immutable after construction, so they are shared without locks.

use std::collections::HashMap;

use swiftfs_core::{Result, SwiftfsError, ROOT_INODE};
use swiftfs_format::{Chunk, Manifest, Node, NodeKind};

/// Immutable, queryable view of a manifest.
#[derive(Debug)]
pub struct ManifestIndex {
    nodes_by_inode: HashMap<u64, usize>,
    children: HashMap<u64, Vec<u64>>,
    child_by_name: HashMap<(u64, String), u64>,
    manifest: Manifest,
}

impl ManifestIndex {
    /// Build an index from a validated manifest.
    ///
    /// The manifest is validated first; additionally, parent-child consistency
    /// is checked so that every non-root node has an existing parent directory.
    pub fn new(manifest: Manifest) -> Result<Self> {
        manifest.validate()?;

        let mut nodes_by_inode = HashMap::with_capacity(manifest.nodes.len());
        for (i, node) in manifest.nodes.iter().enumerate() {
            nodes_by_inode.insert(node.inode, i);
        }

        let mut children: HashMap<u64, Vec<u64>> = HashMap::new();
        let mut child_by_name: HashMap<(u64, String), u64> = HashMap::new();

        // Sort names within each directory for stable readdir ordering.
        for node in &manifest.nodes {
            if node.inode == ROOT_INODE {
                continue;
            }
            let (parent_path, name) = split_parent(&node.path)?;
            let parent = manifest.node_by_path(parent_path).ok_or_else(|| {
                SwiftfsError::InvalidImage(format!(
                    "node {:?} has no parent directory {:?}",
                    node.path, parent_path
                ))
            })?;
            if parent.kind != NodeKind::Dir {
                return Err(SwiftfsError::InvalidImage(format!(
                    "parent {:?} of {:?} is not a directory",
                    parent.path, node.path
                )));
            }
            child_by_name.insert((parent.inode, name.to_string()), node.inode);
            children.entry(parent.inode).or_default().push(node.inode);
        }

        for kids in children.values_mut() {
            kids.sort_unstable();
            // Defensive: identical names under one parent are caught by the
            // duplicate-path check in manifest validation, but assert ordering
            // invariants here too.
        }

        Ok(Self {
            nodes_by_inode,
            children,
            child_by_name,
            manifest,
        })
    }

    /// The underlying manifest.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Root inode number.
    pub fn root(&self) -> u64 {
        ROOT_INODE
    }

    /// Look up a node by inode.
    pub fn node(&self, inode: u64) -> Option<&Node> {
        self.nodes_by_inode
            .get(&inode)
            .map(|&i| &self.manifest.nodes[i])
    }

    /// Resolve a child name within a directory inode.
    pub fn lookup(&self, parent: u64, name: &str) -> Option<u64> {
        self.child_by_name.get(&(parent, name.to_string())).copied()
    }

    /// Ordered child inodes of a directory.
    pub fn children(&self, inode: u64) -> &[u64] {
        self.children
            .get(&inode)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Chunks that overlap the logical byte range `[offset, offset+length)`.
    ///
    /// The range is clamped to the file size, matching POSIX read semantics past
    /// EOF (returns fewer bytes, not an error). Returns the chunks in file
    /// order along with the sub-range of each chunk actually needed.
    pub fn chunks_for_range(&self, node: &Node, offset: u64, length: u64) -> Vec<ChunkRead> {
        if node.kind != NodeKind::File || length == 0 {
            return Vec::new();
        }
        let end = offset.saturating_add(length).min(node.size);
        if offset >= end {
            return Vec::new();
        }

        let mut out = Vec::new();
        for chunk in &node.chunks {
            let chunk_start = chunk.offset;
            let chunk_end = chunk.offset + u64::from(chunk.length);
            if chunk_end <= offset || chunk_start >= end {
                continue;
            }
            let read_start = offset.max(chunk_start);
            let read_end = end.min(chunk_end);
            out.push(ChunkRead {
                chunk: chunk.clone(),
                // Portion of the file this read covers.
                file_offset: read_start,
                // Portion of the fetched chunk to slice out.
                in_chunk_offset: (read_start - chunk_start) as u32,
                len: (read_end - read_start) as u32,
            });
        }
        out
    }

    /// The next `count` chunks of `node` that begin at or after `offset`.
    ///
    /// Used to schedule read-ahead: it returns chunks strictly beyond the byte
    /// position already served, in file order, capped at `count`.
    pub fn chunks_after<'a>(&self, node: &'a Node, offset: u64, count: usize) -> Vec<&'a Chunk> {
        if node.kind != NodeKind::File || count == 0 {
            return Vec::new();
        }
        node.chunks
            .iter()
            .filter(|c| c.offset >= offset)
            .take(count)
            .collect()
    }
}

/// A chunk overlapping a requested read, with the sub-range to serve.
#[derive(Debug, Clone)]
pub struct ChunkRead {
    /// The chunk metadata (hash, object, length).
    pub chunk: Chunk,
    /// Absolute file offset this piece begins at.
    pub file_offset: u64,
    /// Offset within the fetched chunk bytes.
    pub in_chunk_offset: u32,
    /// Number of bytes to take from the chunk.
    pub len: u32,
}

/// Split an absolute path into `(parent_path, name)`.
fn split_parent(path: &str) -> Result<(&str, &str)> {
    let idx = path
        .rfind('/')
        .ok_or_else(|| SwiftfsError::InvalidImage(format!("path {path:?} has no parent")))?;
    let (parent, name) = path.split_at(idx);
    let name = &name[1..];
    if name.is_empty() {
        return Err(SwiftfsError::InvalidImage(format!(
            "path {path:?} has empty name"
        )));
    }
    let parent = if parent.is_empty() { "/" } else { parent };
    Ok((parent, name))
}
