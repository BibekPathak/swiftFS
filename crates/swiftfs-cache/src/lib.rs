//! Multi-level caching and single-flight request coalescing.
//!
//! # Design
//!
//! The cache key is the **content hash** of a chunk (BLAKE3 hex). This means two
//! different files (or offsets) that share identical bytes share one cache
//! entry. Bytes are verified against the key before insertion, so a cache entry
//! can only ever hold data that hashes to its key.
//!
//! Two tiers:
//!
//! - [`RamCache`]: bounded in-memory LRU. Capacity is enforced **strictly**:
//!   the inner lock is held across eviction so the byte total never exceeds the
//!   configured limit.
//! - [`DiskCache`]: optional content-addressed files with atomic publication
//!   (write to a temp file, fsync, rename). Capacity is enforced approximately:
//!   it is checked on insertion, not under a global lock, so the on-disk total
//!   may briefly overshoot. This is documented rather than hidden.
//!
//! [`ChunkFetcher`] wraps a backend and provides **single-flight** behavior: if
//! N tasks request the same missing hash concurrently, exactly one backend fetch
//! runs and all N share the result. The in-flight map is cleaned up on both
//! success and failure.

#![forbid(unsafe_code)]

pub mod disk;
pub mod fetcher;
pub mod ram;

pub use disk::{DiskCache, DiskMetrics};
pub use fetcher::{ChunkFetcher, FetcherConfig, FetcherMetrics};
pub use ram::{CacheMetrics, RamCache};

/// A shared, reference-counted chunk payload. Cheap to clone out of a cache.
pub type ChunkBytes = std::sync::Arc<Vec<u8>>;
