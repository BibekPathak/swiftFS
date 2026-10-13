//! Read-ahead (prefetch) heuristic for the FUSE read path.
//!
//! SwiftFS is read-only and its files are immutable, so detecting sequential
//! access and warming the cache for the chunks that follow the current read is
//! a pure win: it cannot change data, only which chunks are resident.
//!
//! The heuristic is deliberately small and self-contained:
//!
//! - A read is considered *sequential* when it begins at or just after the
//!   previous read for the same inode ended (within [`READ_AHEAD_WINDOW`]).
//! - Only reads of at least [`READ_AHEAD_MIN_READ`] bytes trigger read-ahead, to
//!   avoid amplifying many tiny random reads.
//! - When triggered, up to [`READ_AHEAD_CHUNKS`] chunks *following* the chunk(s)
//!   just read are fetched in the background.
//!
//! Prefetch fetches go through the same [`ChunkFetcher`](swiftfs_cache::ChunkFetcher)
//! as foreground reads, so they are coalesced with any concurrent real read and
//! share the single-flight in-flight map. They run on the daemon's Tokio
//! runtime and never block the FUSE worker thread that issued the read.

use std::collections::HashMap;
use std::sync::Mutex;

/// Number of chunks to prefetch ahead of a detected sequential read.
pub const READ_AHEAD_CHUNKS: usize = 2;

/// A subsequent read is treated as sequential if it starts within this many
/// bytes of where the previous read on the same inode ended.
pub const READ_AHEAD_WINDOW: u64 = 256 * 1024;

/// Reads smaller than this do not trigger read-ahead.
pub const READ_AHEAD_MIN_READ: u32 = 4 * 1024;

/// Max number of inodes tracked at once; bounded so the map cannot grow without
/// limit under adversarial access patterns.
const MAX_TRACKED_INODES: usize = 4096;

/// Per-inode sequential-access state.
#[derive(Debug, Default)]
struct ReadState {
    /// End offset (exclusive) of the most recent read observed.
    last_end: u64,
    /// Whether the most recent read looked sequential.
    sequential: bool,
}

/// Tracks recent read positions to decide when to prefetch.
#[derive(Debug, Default)]
pub struct ReadAhead {
    inner: Mutex<HashMap<u64, ReadState>>,
}

impl ReadAhead {
    /// Create an empty tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a read and decide whether it is sequential.
    ///
    /// Returns `true` when read-ahead should be scheduled for `ino`.
    pub fn observe(&self, ino: u64, offset: u64, size: u32) -> bool {
        if size < READ_AHEAD_MIN_READ {
            return false;
        }
        let mut map = self.inner.lock().expect("read-ahead mutex poisoned");

        // Bound the tracked set; clear it if it has grown too large. This is a
        // coarse but safe reset — a dropped hint only costs one cold read.
        if map.len() >= MAX_TRACKED_INODES && !map.contains_key(&ino) {
            map.clear();
        }

        let end = offset.saturating_add(u64::from(size));
        let entry = map.entry(ino).or_default();
        let sequential = if entry.last_end == 0 {
            true
        } else {
            offset >= entry.last_end && offset.saturating_sub(entry.last_end) <= READ_AHEAD_WINDOW
        };
        entry.last_end = end;
        entry.sequential = sequential;
        sequential
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_read_is_sequential() {
        let ra = ReadAhead::new();
        assert!(ra.observe(1, 0, READ_AHEAD_MIN_READ));
    }

    #[test]
    fn tiny_reads_do_not_trigger() {
        let ra = ReadAhead::new();
        assert!(!ra.observe(1, 0, READ_AHEAD_MIN_READ - 1));
    }

    #[test]
    fn contiguous_reads_are_sequential() {
        let ra = ReadAhead::new();
        assert!(ra.observe(1, 0, 8192));
        assert!(ra.observe(1, 8192, 8192));
        assert!(ra.observe(1, 16384, 8192));
    }

    #[test]
    fn distant_read_is_not_sequential() {
        let ra = ReadAhead::new();
        assert!(ra.observe(1, 0, 8192));
        assert!(!ra.observe(1, 8192 + READ_AHEAD_WINDOW + 1, 8192));
    }

    #[test]
    fn per_inode_state_is_independent() {
        let ra = ReadAhead::new();
        assert!(ra.observe(1, 0, 8192));
        assert!(ra.observe(2, 0, 8192));
        assert!(ra.observe(1, 8192, 8192));
    }
}
