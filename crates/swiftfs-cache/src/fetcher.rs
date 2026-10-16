//! Single-flight chunk fetching over a backend, with RAM and disk caches.
//!
//! # Coalescing
//!
//! Concurrent requests for the same content hash share one backend fetch. The
//! in-flight map holds a `Shared` future keyed by hash; the first caller inserts
//! it and performs the fetch, later callers clone the shared handle. When the
//! fetch completes the entry is removed by the fetch future itself, so both
//! success and failure clean up. Because `Shared` yields a shared `Result`, all
//! waiters observe the same outcome.
//!
//! # Integrity
//!
//! Bytes fetched from the backend are hashed and compared to the requested key
//! **before** being inserted into either cache. A mismatch is an error and
//! nothing is cached, so a corrupt response cannot poison the cache.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::future::{BoxFuture, FutureExt, Shared};
use swiftfs_core::metrics::{Counter, Histogram};
use swiftfs_core::{Result, SwiftfsError};
use swiftfs_storage::{ChunkLocation, ChunkStore};
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::{ChunkBytes, DiskCache, RamCache};

/// Fetcher configuration.
#[derive(Debug, Clone)]
pub struct FetcherConfig {
    /// Maximum concurrent backend fetches. Additional callers for the same
    /// missing chunk join the single-flight future; unrelated chunks wait on the
    /// semaphore rather than spawning unbounded backend work.
    pub max_concurrent_fetches: usize,
    /// Timeout applied around a single backend fetch.
    pub fetch_timeout: Duration,
}

impl Default for FetcherConfig {
    fn default() -> Self {
        Self {
            max_concurrent_fetches: 32,
            fetch_timeout: Duration::from_secs(15),
        }
    }
}

/// Fetcher metrics.
#[derive(Debug, Default)]
pub struct FetcherMetrics {
    /// Requests for a chunk (cache lookups).
    pub requests: Counter,
    /// RAM cache hits.
    pub ram_hits: Counter,
    /// Disk cache hits.
    pub disk_hits: Counter,
    /// Backend fetches actually performed (after coalescing).
    pub backend_fetches: Counter,
    /// Actual bytes returned by the backend and verified (sum of chunk lengths,
    /// excluding cache hits and coalesced joins). This is the true measure of
    /// backend data movement, as opposed to `backend_fetches * chunk_size`.
    pub backend_bytes: Counter,
    /// Requests that joined an in-flight fetch instead of starting a new one.
    pub coalesced: Counter,
    /// Integrity failures.
    pub integrity_failures: Counter,
    /// Backend/timeout failures.
    pub fetch_failures: Counter,
    /// End-to-end chunk retrieval latency histogram.
    pub latency: Histogram,
}

/// The coalesced result. Errors are wrapped in `Arc` so the shared future's
/// output is `Clone`, which `Shared` requires.
type SharedResult = std::result::Result<ChunkBytes, Arc<SwiftfsError>>;
type SharedFetch = Shared<BoxFuture<'static, SharedResult>>;

/// A cache-coalescing chunk fetcher.
pub struct ChunkFetcher {
    backend: Arc<dyn ChunkStore>,
    ram: Arc<RamCache>,
    disk: Option<Arc<DiskCache>>,
    config: FetcherConfig,
    inflight: Arc<Mutex<HashMap<String, SharedFetch>>>,
    semaphore: Arc<tokio::sync::Semaphore>,
    metrics: Arc<FetcherMetrics>,
}

impl ChunkFetcher {
    /// Construct a fetcher.
    pub fn new(
        backend: Arc<dyn ChunkStore>,
        ram: Arc<RamCache>,
        disk: Option<Arc<DiskCache>>,
        config: FetcherConfig,
    ) -> Self {
        let permits = config.max_concurrent_fetches.max(1);
        Self {
            backend,
            ram,
            disk,
            config,
            inflight: Arc::new(Mutex::new(HashMap::new())),
            semaphore: Arc::new(tokio::sync::Semaphore::new(permits)),
            metrics: Arc::new(FetcherMetrics::default()),
        }
    }

    /// Shared metrics.
    pub fn metrics(&self) -> Arc<FetcherMetrics> {
        Arc::clone(&self.metrics)
    }

    /// RAM cache handle (for tests and reporting).
    pub fn ram(&self) -> &Arc<RamCache> {
        &self.ram
    }

    /// Disk cache handle, if configured.
    pub fn disk(&self) -> Option<&Arc<DiskCache>> {
        self.disk.as_ref()
    }

    /// Fetch the chunk identified by `hash`, using caches and coalescing.
    ///
    /// `location` says where to fetch the bytes if they are not cached; its
    /// object/offset/length are backend-specific and need only produce bytes
    /// that hash to `hash`.
    pub async fn get(&self, hash: &str, location: &ChunkLocation) -> Result<ChunkBytes> {
        let started = std::time::Instant::now();
        self.metrics.requests.inc();

        // RAM tier.
        if let Some(hit) = self.ram.get(hash) {
            self.metrics.ram_hits.inc();
            self.metrics.latency.observe_duration(started.elapsed());
            return Ok(hit);
        }

        // Disk tier: verify before trusting, then promote into RAM.
        if let Some(disk) = &self.disk {
            if let Some(data) = disk.get(hash) {
                let actual = blake3::hash(&data).to_hex().to_string();
                if actual == hash {
                    self.metrics.disk_hits.inc();
                    let arc: ChunkBytes = Arc::new(data);
                    self.ram.insert(hash, (*arc).clone());
                    self.metrics.latency.observe_duration(started.elapsed());
                    return Ok(arc);
                }
                warn!(hash, "disk cache entry failed hash; evicting");
                disk.remove(hash);
            }
        }

        // Coalesce: join an existing in-flight fetch or start one.
        let shared = {
            let mut map = self.inflight.lock().await;
            if let Some(existing) = map.get(hash) {
                self.metrics.coalesced.inc();
                existing.clone()
            } else {
                let fut = self.spawn_fetch(hash.to_string(), location.clone());
                let shared: SharedFetch = fut.boxed().shared();
                map.insert(hash.to_string(), shared.clone());
                shared
            }
        };

        let result = shared.await;
        self.metrics.latency.observe_duration(started.elapsed());
        result.map_err(|e| match &*e {
            SwiftfsError::InvalidImage(m) => SwiftfsError::InvalidImage(m.clone()),
            SwiftfsError::PathRejected(m) => SwiftfsError::PathRejected(m.clone()),
            SwiftfsError::UnsupportedFileType(m) => SwiftfsError::UnsupportedFileType(m.clone()),
            SwiftfsError::ChunkNotFound(m) => SwiftfsError::ChunkNotFound(m.clone()),
            SwiftfsError::IntegrityFailure(m) => SwiftfsError::IntegrityFailure(m.clone()),
            SwiftfsError::Storage(m) => SwiftfsError::Storage(m.clone()),
            SwiftfsError::Io(io) => SwiftfsError::Storage(format!("io: {io}")),
        })
    }

    /// Build the future that performs (and cleans up) a single backend fetch.
    fn spawn_fetch(
        &self,
        hash: String,
        location: ChunkLocation,
    ) -> BoxFuture<'static, SharedResult> {
        let backend = Arc::clone(&self.backend);
        let ram = Arc::clone(&self.ram);
        let disk = self.disk.clone();
        let semaphore = Arc::clone(&self.semaphore);
        let inflight = Arc::clone(&self.inflight);
        let metrics = Arc::clone(&self.metrics);
        let timeout = self.config.fetch_timeout;

        async move {
            // Bounded concurrency for backend work.
            let _permit = semaphore
                .acquire_owned()
                .await
                .map_err(|_| Arc::new(SwiftfsError::Storage("fetch semaphore closed".into())))?;

            let outcome = tokio::time::timeout(timeout, backend.fetch(&location)).await;

            // Clean up the in-flight entry regardless of outcome.
            {
                let mut map = inflight.lock().await;
                map.remove(&hash);
            }

            let data = match outcome {
                Ok(Ok(data)) => data,
                Ok(Err(e)) => {
                    metrics.fetch_failures.inc();
                    return Err(Arc::new(e));
                }
                Err(_) => {
                    metrics.fetch_failures.inc();
                    return Err(Arc::new(SwiftfsError::Storage(format!(
                        "fetch of {hash} timed out after {timeout:?}"
                    ))));
                }
            };

            // Verify before caching or serving.
            let actual = blake3::hash(&data).to_hex().to_string();
            if actual != hash {
                metrics.integrity_failures.inc();
                return Err(Arc::new(SwiftfsError::IntegrityFailure(format!(
                    "backend returned data hashing to {actual}, expected {hash}"
                ))));
            }

            metrics.backend_fetches.inc();
            metrics.backend_bytes.add(data.len() as u64);

            if let Some(disk) = &disk {
                if let Err(e) = disk.insert(&hash, &data) {
                    // Disk cache is best-effort; a failure must not fail the read.
                    debug!(hash, error = %e, "disk cache insert failed");
                }
            }

            let arc: ChunkBytes = Arc::new(data);
            ram.insert(&hash, (*arc).clone());
            Ok(arc)
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use swiftfs_storage::MemStore;

    /// `backend_bytes` must equal the sum of *actual* returned chunk lengths,
    /// not `fetches * chunk_size`. A short (tail) chunk proves the difference.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backend_bytes_counts_actual_lengths_not_chunk_size() {
        let short = b"tail".to_vec(); // 4 bytes, far below any chunk size
        let hash = blake3::hash(&short).to_hex().to_string();
        let mut objects = HashMap::new();
        objects.insert(hash.clone(), short.clone());
        let store: Arc<dyn ChunkStore> = Arc::new(MemStore::new(objects));

        let ram = Arc::new(RamCache::new(1 << 20));
        let fetcher = ChunkFetcher::new(store, ram, None, FetcherConfig::default());
        let location = ChunkLocation {
            object: hash.clone(),
            offset: 0,
            length: short.len() as u32,
        };

        let got = fetcher.get(&hash, &location).await.expect("fetch");
        assert_eq!(got.len(), short.len());

        let m = fetcher.metrics();
        assert_eq!(m.backend_fetches.get(), 1);
        assert_eq!(m.backend_bytes.get(), short.len() as u64);

        // A second read hits RAM and must not add backend bytes.
        let _ = fetcher.get(&hash, &location).await.expect("cached fetch");
        assert_eq!(m.backend_fetches.get(), 1);
        assert_eq!(m.backend_bytes.get(), short.len() as u64);
    }
}
