//! Bounded in-memory LRU cache keyed by content hash.
//!
//! Capacity is enforced **strictly** in bytes: insertion and eviction happen
//! under a single lock, so the resident byte total never exceeds `capacity`. A
//! single entry larger than the whole capacity is rejected rather than admitted
//! (which would force evicting everything and still not fit).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use swiftfs_core::metrics::{Counter, Gauge, Histogram};

use crate::ChunkBytes;

/// Cache hit/miss/eviction metrics.
#[derive(Debug, Default)]
pub struct CacheMetrics {
    /// Hits served from RAM.
    pub ram_hits: Counter,
    /// Misses in RAM.
    pub ram_misses: Counter,
    /// Entries evicted to respect capacity.
    pub evictions: Counter,
    /// Bytes currently resident.
    pub resident_bytes: Gauge,
    /// Entries inserted.
    pub insertions: Counter,
    /// Entries rejected because a single entry exceeded capacity.
    pub oversized_rejections: Counter,
    /// Lookup latency histogram.
    pub lookup_latency: Histogram,
}

struct Inner {
    /// key -> (bytes, last-used sequence)
    entries: HashMap<String, (ChunkBytes, u64)>,
    /// sequence -> key, ordered for LRU eviction (lowest sequence = oldest)
    order: BTreeMap<u64, String>,
    resident_bytes: u64,
    next_seq: u64,
}

/// Bounded RAM cache.
pub struct RamCache {
    inner: std::sync::Mutex<Inner>,
    capacity: u64,
    metrics: Arc<CacheMetrics>,
}

impl RamCache {
    /// Create a cache bounded to `capacity` bytes.
    pub fn new(capacity: u64) -> Self {
        Self {
            inner: std::sync::Mutex::new(Inner {
                entries: HashMap::new(),
                order: BTreeMap::new(),
                resident_bytes: 0,
                next_seq: 0,
            }),
            capacity,
            metrics: Arc::new(CacheMetrics::default()),
        }
    }

    /// Configured capacity in bytes.
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Shared metrics.
    pub fn metrics(&self) -> Arc<CacheMetrics> {
        Arc::clone(&self.metrics)
    }

    /// Look up a key, updating LRU order on hit.
    pub fn get(&self, key: &str) -> Option<ChunkBytes> {
        let started = std::time::Instant::now();
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let seq = inner.next_seq;
        let result = if inner.entries.contains_key(key) {
            let (data, old_seq) = inner.entries.get(key).map(|(d, s)| (Arc::clone(d), *s)).unwrap();
            inner.order.remove(&old_seq);
            if let Some(entry) = inner.entries.get_mut(key) {
                entry.1 = seq;
            }
            inner.order.insert(seq, key.to_string());
            inner.next_seq += 1;
            Some(data)
        } else {
            None
        };
        let resident = inner.resident_bytes;
        drop(inner);
        self.metrics.resident_bytes.set(resident);

        self.metrics
            .lookup_latency
            .observe_duration(started.elapsed());
        match &result {
            Some(_) => self.metrics.ram_hits.inc(),
            None => self.metrics.ram_misses.inc(),
        }
        result
    }

    /// Insert `data` under `key`. If `data` exceeds capacity it is rejected and
    /// `false` is returned.
    pub fn insert(&self, key: &str, data: Vec<u8>) -> bool {
        let size = data.len() as u64;
        if size > self.capacity {
            self.metrics.oversized_rejections.inc();
            return false;
        }

        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        // Replace an existing entry for the same key.
        if let Some((old, old_seq)) = inner.entries.remove(key) {
            inner.order.remove(&old_seq);
            inner.resident_bytes = inner.resident_bytes.saturating_sub(old.len() as u64);
        }

        // Evict oldest until the new entry fits.
        while inner.resident_bytes + size > self.capacity {
            let oldest = match inner.order.keys().next().copied() {
                Some(s) => s,
                None => break,
            };
            if let Some(evict_key) = inner.order.remove(&oldest) {
                if let Some((evicted, _)) = inner.entries.remove(&evict_key) {
                    inner.resident_bytes =
                        inner.resident_bytes.saturating_sub(evicted.len() as u64);
                    self.metrics.evictions.inc();
                }
            }
        }

        let seq = inner.next_seq;
        inner.next_seq += 1;
        inner.resident_bytes += size;
        inner.entries.insert(key.to_string(), (Arc::new(data), seq));
        inner.order.insert(seq, key.to_string());
        let resident = inner.resident_bytes;
        drop(inner);

        self.metrics.insertions.inc();
        self.metrics.resident_bytes.set(resident);
        true
    }

    /// Remove a key if present.
    pub fn remove(&self, key: &str) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((data, seq)) = inner.entries.remove(key) {
            inner.order.remove(&seq);
            inner.resident_bytes = inner.resident_bytes.saturating_sub(data.len() as u64);
            let resident = inner.resident_bytes;
            drop(inner);
            self.metrics.resident_bytes.set(resident);
            true
        } else {
            false
        }
    }

    /// Bytes currently resident.
    pub fn resident_bytes(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .resident_bytes
    }

    /// Number of resident entries.
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Clear all entries.
    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.entries.clear();
        inner.order.clear();
        inner.resident_bytes = 0;
        drop(inner);
        self.metrics.resident_bytes.set(0);
    }
}
