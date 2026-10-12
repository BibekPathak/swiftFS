//! Chunk storage backends.
//!
//! Defines a backend-agnostic async trait so the filesystem can fetch chunk
//! bytes without depending on a specific transport. Phase 3 implements the
//! local directory backend; the HTTP byte-range backend arrives in Phase 4.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use swiftfs_core::{Result, SwiftfsError};
use tracing::debug;

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

/// A backend that can fetch a [`ChunkLocation`]'s bytes.
///
/// Implementations must return exactly `location.length` bytes on success or an
/// error. Callers are responsible for verifying content hashes before trusting
/// the bytes.
#[async_trait]
pub trait ChunkStore: Send + Sync + 'static {
    /// Fetch the bytes for `location`.
    async fn fetch(&self, location: &ChunkLocation) -> Result<Vec<u8>>;

    /// Human-readable backend name for logs and metrics labels.
    fn name(&self) -> &'static str;
}

/// Snapshot of storage counters. Cheap to read; used for metrics.
#[derive(Debug, Clone, Copy, Default)]
pub struct StorageStats {
    /// Number of successful fetches.
    pub fetches: u64,
    /// Number of failed fetches.
    pub errors: u64,
    /// Total bytes successfully fetched.
    pub bytes: u64,
}

/// Shared atomic counters used by backends.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    pub fetches: AtomicU64,
    pub errors: AtomicU64,
    pub bytes: AtomicU64,
}

impl Counters {
    pub fn snapshot(&self) -> StorageStats {
        StorageStats {
            fetches: self.fetches.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
        }
    }
}

/// Local directory backend. Chunk objects are files under a chunks directory,
/// named by object id. In format v1 a chunk occupies the whole object.
#[derive(Debug)]
pub struct LocalStore {
    chunks_dir: PathBuf,
    counters: Arc<Counters>,
}

impl LocalStore {
    /// Create a backend rooted at `chunks_dir`.
    pub fn new(chunks_dir: impl Into<PathBuf>) -> Self {
        Self {
            chunks_dir: chunks_dir.into(),
            counters: Arc::new(Counters::default()),
        }
    }

    /// Create a backend for an image root, using its `chunks/` subdirectory.
    pub fn for_image(root: impl AsRef<Path>) -> Self {
        Self::new(root.as_ref().join("chunks"))
    }

    /// Current storage counters.
    pub fn stats(&self) -> StorageStats {
        self.counters.snapshot()
    }

    /// Path of an object within the backend.
    fn object_path(&self, object: &str) -> PathBuf {
        self.chunks_dir.join(object)
    }
}

#[async_trait]
impl ChunkStore for LocalStore {
    async fn fetch(&self, location: &ChunkLocation) -> Result<Vec<u8>> {
        let path = self.object_path(&location.object);
        let offset = location.offset;
        let length = location.length;

        // Local file reads are synchronous but small (one chunk). Run them on
        // Tokio's blocking pool so a runtime worker thread is never blocked.
        // This works on both current-thread and multi-thread runtimes.
        let join = tokio::task::spawn_blocking(move || read_range(&path, offset, length));
        let result = match join.await {
            Ok(inner) => inner,
            Err(e) => Err(SwiftfsError::Storage(format!(
                "local read task failed: {e}"
            ))),
        };

        match &result {
            Ok(data) => {
                self.counters.fetches.fetch_add(1, Ordering::Relaxed);
                self.counters
                    .bytes
                    .fetch_add(data.len() as u64, Ordering::Relaxed);
                debug!(object = %location.object, len = data.len(), "local fetch");
            }
            Err(_) => {
                self.counters.errors.fetch_add(1, Ordering::Relaxed);
            }
        }
        result
    }

    fn name(&self) -> &'static str {
        "local"
    }
}

fn read_range(path: &Path, offset: u64, length: u32) -> Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            SwiftfsError::ChunkNotFound(format!("{}", path.display()))
        } else {
            SwiftfsError::Storage(format!("open {}: {e}", path.display()))
        }
    })?;

    let file_len = file
        .metadata()
        .map_err(|e| SwiftfsError::Storage(format!("stat {}: {e}", path.display())))?
        .len();

    let end = offset
        .checked_add(u64::from(length))
        .ok_or_else(|| SwiftfsError::Storage("chunk range overflow".into()))?;
    if end > file_len {
        return Err(SwiftfsError::IntegrityFailure(format!(
            "{}: requested {}..{} but object is {} bytes",
            path.display(),
            offset,
            end,
            file_len
        )));
    }

    file.seek(SeekFrom::Start(offset))
        .map_err(|e| SwiftfsError::Storage(format!("seek {}: {e}", path.display())))?;

    let mut buf = vec![0u8; length as usize];
    file.read_exact(&mut buf)
        .map_err(|e| SwiftfsError::Storage(format!("read {}: {e}", path.display())))?;
    Ok(buf)
}
