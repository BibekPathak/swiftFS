//! Content-addressed disk cache with atomic publication.
//!
//! Each chunk is stored as a file named by its content hash. Writes go to a
//! unique temporary file, are fsynced, then renamed into place. A crash
//! therefore cannot leave a partially written file under a valid hash name: the
//! reader either finds a complete file or nothing.
//!
//! Capacity is enforced **approximately**: it is checked on insertion without a
//! global lock across the whole cache, so under concurrency the on-disk total
//! may briefly exceed the configured limit before the next sweep. This is
//! documented rather than hidden. Eviction removes least-recently-modified
//! entries until the total is back under the limit.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use swiftfs_core::metrics::{Counter, Gauge};
use swiftfs_core::{Result, SwiftfsError};
use tracing::{debug, warn};

/// Disk cache metrics.
#[derive(Debug, Default)]
pub struct DiskMetrics {
    /// Reads served from disk.
    pub hits: Counter,
    /// Misses on disk.
    pub misses: Counter,
    /// Files written.
    pub writes: Counter,
    /// Files evicted.
    pub evictions: Counter,
    /// Bytes currently stored.
    pub resident_bytes: Gauge,
    /// Write/rename failures.
    pub errors: Counter,
}

/// A content-addressed disk cache.
pub struct DiskCache {
    dir: PathBuf,
    capacity: u64,
    metrics: Arc<DiskMetrics>,
    /// Monotonic counter to make temp file names unique per process.
    tmp_seq: AtomicU64,
}

impl DiskCache {
    /// Create (and populate the byte total of) a disk cache rooted at `dir`.
    pub fn new(dir: impl Into<PathBuf>, capacity: u64) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        let cache = Self {
            dir,
            capacity,
            metrics: Arc::new(DiskMetrics::default()),
            tmp_seq: AtomicU64::new(0),
        };
        cache.metrics.resident_bytes.set(cache.total_bytes());
        Ok(cache)
    }

    /// Root directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Shared metrics.
    pub fn metrics(&self) -> Arc<DiskMetrics> {
        Arc::clone(&self.metrics)
    }

    fn path_for(&self, key: &str) -> PathBuf {
        self.dir.join(key)
    }

    /// Total bytes currently stored.
    pub fn total_bytes(&self) -> u64 {
        match fs::read_dir(&self.dir) {
            Ok(entries) => entries
                .filter_map(|e| e.ok())
                .filter_map(|e| e.metadata().ok())
                .filter(|m| m.is_file())
                .map(|m| m.len())
                .sum(),
            Err(_) => 0,
        }
    }

    /// Look up a key, returning its bytes if present and complete.
    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        let path = self.path_for(key);
        match fs::read(&path) {
            Ok(data) => {
                self.metrics.hits.inc();
                Some(data)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.metrics.misses.inc();
                None
            }
            Err(e) => {
                warn!(key, error = %e, "disk cache read failed; treating as miss");
                self.metrics.misses.inc();
                None
            }
        }
    }

    /// Atomically publish `data` under `key`, then enforce capacity.
    pub fn insert(&self, key: &str, data: &[u8]) -> Result<()> {
        if data.len() as u64 > self.capacity {
            return Ok(()); // too big to ever cache; skip silently
        }
        let final_path = self.path_for(key);
        if final_path.exists() {
            return Ok(()); // already cached
        }

        let seq = self.tmp_seq.fetch_add(1, Ordering::Relaxed);
        let tmp_path = self
            .dir
            .join(format!(".tmp.{}.{}.{}", std::process::id(), seq, key));

        let write_result = (|| -> std::io::Result<()> {
            let mut f = fs::File::create(&tmp_path)?;
            f.write_all(data)?;
            f.sync_all()?;
            Ok(())
        })();

        if let Err(e) = write_result {
            let _ = fs::remove_file(&tmp_path);
            self.metrics.errors.inc();
            warn!(key, error = %e, "disk cache write failed");
            return Err(SwiftfsError::Io(e));
        }

        // Atomic publication. If the target appeared meanwhile, discard ours.
        if let Err(e) = fs::rename(&tmp_path, &final_path) {
            let _ = fs::remove_file(&tmp_path);
            self.metrics.errors.inc();
            return Err(SwiftfsError::Io(e));
        }

        self.metrics.writes.inc();
        self.metrics.resident_bytes.set(self.total_bytes());
        self.enforce_capacity()?;
        Ok(())
    }

    /// Evict least-recently-modified files until under capacity.
    pub fn enforce_capacity(&self) -> Result<()> {
        let mut total = self.total_bytes();
        if total <= self.capacity {
            return Ok(());
        }

        // Collect file metadata, oldest first.
        let mut files: Vec<(std::time::SystemTime, PathBuf, u64)> = fs::read_dir(&self.dir)?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                if !meta.is_file() {
                    return None;
                }
                let name = e.file_name();
                // Never count or evict in-progress temp files as cache entries.
                if name.to_string_lossy().starts_with(".tmp.") {
                    return None;
                }
                let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                Some((mtime, e.path(), meta.len()))
            })
            .collect();
        files.sort_by_key(|(mtime, _, _)| *mtime);

        for (_, path, len) in files {
            if total <= self.capacity {
                break;
            }
            match fs::remove_file(&path) {
                Ok(()) => {
                    total = total.saturating_sub(len);
                    self.metrics.evictions.inc();
                    debug!(path = %path.display(), "disk cache eviction");
                }
                Err(e) => warn!(path = %path.display(), error = %e, "disk eviction failed"),
            }
        }
        self.metrics.resident_bytes.set(total);
        Ok(())
    }

    /// Remove a specific key.
    pub fn remove(&self, key: &str) {
        let _ = fs::remove_file(self.path_for(key));
        self.metrics.resident_bytes.set(self.total_bytes());
    }
}
