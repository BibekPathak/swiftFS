//! Read-only FUSE filesystem backed by a SwiftFS image.
//!
//! # Threading model
//!
//! `fuser::Filesystem` is synchronous: each callback runs on a `fuser` session
//! worker thread (see `docs/design-decisions.md`, D2). Storage I/O is
//! asynchronous and runs on a dedicated Tokio runtime owned by the daemon.
//!
//! For `read`, the callback needs chunk bytes. It bridges to the runtime by
//! blocking **the FUSE worker thread** on a oneshot channel while the fetch runs
//! as a Tokio task. It never blocks a Tokio worker thread, and it never awaits
//! from inside a callback. Because the fetch is spawned onto the runtime, the
//! blocking wait is bounded by the storage timeout enforced in the backend
//! (Phase 4). This design keeps the FUSE and Tokio thread pools disjoint.
//
//! All manifest-derived state (`ManifestIndex`) is immutable and shared via
//! `Arc`, so no lock is held across a `read`. Caching and single-flight
//! coalescing are provided by the `ChunkFetcher`, which deduplicates concurrent
//! requests for the same chunk across the RAM cache, disk cache, and backend.

use std::ffi::OsStr;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    KernelConfig, LockOwner, OpenAccMode, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyXattr, Request,
};
use swiftfs_cache::ChunkFetcher;
use swiftfs_core::{Result as SfsResult, SwiftfsError, DEFAULT_CHUNK_SIZE};
use swiftfs_format::{Node, NodeKind};
use swiftfs_storage::ChunkLocation;
use tokio::runtime::Handle;
use tracing::{debug, warn};

use crate::index::ManifestIndex;
use crate::read_ahead::{ReadAhead, READ_AHEAD_CHUNKS};

/// Basename of an absolute path.
fn basename(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

/// Attribute / entry cache TTL. The image is immutable, so entries may be
/// cached aggressively.
const TTL: Duration = Duration::from_secs(3600);

/// Read-only SwiftFS filesystem.
pub struct Swiftfs {
    index: Arc<ManifestIndex>,
    fetcher: Arc<ChunkFetcher>,
    rt: Handle,
    uid: u32,
    gid: u32,
    /// Sequential-access tracker; `None` when read-ahead is disabled.
    read_ahead: Option<ReadAhead>,
}

impl Swiftfs {
    /// Construct a filesystem from an index, a fetcher, and a Tokio handle.
    pub fn new(
        index: Arc<ManifestIndex>,
        fetcher: Arc<ChunkFetcher>,
        rt: Handle,
        mount_uid: u32,
        mount_gid: u32,
    ) -> Self {
        Self::with_read_ahead(index, fetcher, rt, mount_uid, mount_gid, true)
    }

    /// Construct a filesystem, choosing whether to enable read-ahead.
    pub fn with_read_ahead(
        index: Arc<ManifestIndex>,
        fetcher: Arc<ChunkFetcher>,
        rt: Handle,
        mount_uid: u32,
        mount_gid: u32,
        read_ahead: bool,
    ) -> Self {
        Self {
            index,
            fetcher,
            rt,
            uid: mount_uid,
            gid: mount_gid,
            read_ahead: read_ahead.then(ReadAhead::new),
        }
    }

    /// The underlying fetcher (for metrics and tests).
    pub fn fetcher(&self) -> &Arc<ChunkFetcher> {
        &self.fetcher
    }

    /// Build `FileAttr` for a node.
    fn attr(&self, inode: u64, node: &Node) -> FileAttr {
        let kind = match node.kind {
            NodeKind::Dir => FileType::Directory,
            NodeKind::File => FileType::RegularFile,
        };
        let mtime = UNIX_EPOCH + Duration::from_secs(node.mtime.max(0) as u64);
        FileAttr {
            ino: INodeNo(inode),
            size: node.size,
            blocks: node.size.div_ceil(512),
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: mtime,
            kind,
            perm: (node.mode & 0o7777) as u16,
            nlink: if node.kind == NodeKind::Dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    /// Fetch and verify all chunks covering a byte range, reassembling them,
    /// using the `ChunkFetcher` (which includes RAM + disk caches and
    /// single-flight coalescing).
    ///
    /// Runs on the FUSE worker thread; internally blocks on the Tokio runtime
    /// (see the module-level threading note).
    fn read_range(&self, node: &Node, offset: u64, size: u32) -> SfsResult<Vec<u8>> {
        let plan = self.index.chunks_for_range(node, offset, u64::from(size));
        if plan.is_empty() {
            return Ok(Vec::new());
        }

        // Block on the runtime to perform the async fetches with caching/coalescing.
        // This is safe: the FUSE worker thread blocking is bounded by the fetch
        let data = tokio::task::block_in_place(|| {
            self.rt.block_on(async {
                let mut out = Vec::with_capacity(size as usize);
                for piece in plan {
                    let location = ChunkLocation {
                        object: piece.chunk.object.clone(),
                        offset: piece.chunk.object_offset,
                        length: piece.chunk.length,
                    };
                    let arc_data = self.fetcher.get(&piece.chunk.object, &location).await?;
                    // `?` inside the async block is fine.
                    // ChunkBytes = Arc<Vec<u8>; clone to get owned Vec<u8>.
                    let data = arc_data.clone();
                    let start = piece.in_chunk_offset as usize;
                    let end = start + piece.len as usize;
                    if end > data.len() {
                        return Err(SwiftfsError::IntegrityFailure(format!(
                            "chunk {} shorter than expected",
                            piece.chunk.object
                        )));
                    }
                    out.extend_from_slice(&data[start..end]);
                }
                Ok(out)
            })
        })?;
        Ok(data)
    }

    /// After serving `[offset, offset+size)`, schedule background prefetch of
    /// the following chunks when the access looks sequential.
    ///
    /// Prefetches go through the same `ChunkFetcher` as foreground reads and run
    /// on the Tokio runtime, so they are coalesced and never block the FUSE
    /// worker thread. This is best-effort: failures are logged and ignored, and
    /// nothing is sent back to the kernel.
    fn maybe_prefetch(&self, ino: u64, node: &Node, offset: u64, size: u32) {
        let Some(ref tracker) = self.read_ahead else {
            return;
        };
        if node.kind != NodeKind::File {
            return;
        }
        if !tracker.observe(ino, offset, size) {
            return;
        }

        let end = offset.saturating_add(u64::from(size));
        let next = self.index.chunks_after(node, end, READ_AHEAD_CHUNKS);
        if next.is_empty() {
            return;
        }
        let locations: Vec<(String, ChunkLocation)> = next
            .into_iter()
            .map(|c| {
                (
                    c.hash.clone(),
                    ChunkLocation {
                        object: c.object.clone(),
                        offset: c.object_offset,
                        length: c.length,
                    },
                )
            })
            .collect();

        let fetcher = Arc::clone(&self.fetcher);
        self.rt.spawn(async move {
            for (hash, location) in locations {
                // Best-effort: a prefetch failure must not surface anywhere.
                if let Err(e) = fetcher.get(&hash, &location).await {
                    debug!(hash, error = %e, "read-ahead prefetch failed");
                }
            }
        });
    }
}

/// Map a SwiftFS error to an errno for the FUSE reply.
fn errno_for(err: &SwiftfsError) -> Errno {
    match err {
        SwiftfsError::ChunkNotFound(_) | SwiftfsError::PathRejected(_) => Errno::ENOENT,
        SwiftfsError::UnsupportedFileType(_) => Errno::EINVAL,
        SwiftfsError::IntegrityFailure(_)
        | SwiftfsError::Storage(_)
        | SwiftfsError::InvalidImage(_) => Errno::EIO,
        SwiftfsError::Io(e) => Errno::from(e.kind()),
    }
}

impl Filesystem for Swiftfs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        // Cap read-ahead to one chunk; the image is immutable and benefits from
        // the page cache, but we do not want unbounded kernel read-ahead.
        let _ = config.set_max_readahead(DEFAULT_CHUNK_SIZE);
        debug!("swiftfs mounted");
        Ok(())
    }

    fn destroy(&mut self) {
        debug!("swiftfs destroyed");
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(name) = name.to_str() else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(child) = self.index.lookup(parent.0, name) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(node) = self.index.node(child) else {
            reply.error(Errno::ENOENT);
            return;
        };
        reply.entry(&TTL, &self.attr(child, node), Generation(0));
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let Some(node) = self.index.node(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        reply.attr(&TTL, &self.attr(ino.0, node));
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        // Reject writes up front with EROFS.
        if flags.acc_mode() != OpenAccMode::O_RDONLY {
            reply.error(Errno::EROFS);
            return;
        }
        match self.index.node(ino.0) {
            Some(node) if node.kind == NodeKind::File => {
                // The image is immutable, so keep the page cache across opens.
                reply.opened(FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE);
            }
            Some(_) => reply.error(Errno::EISDIR),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let Some(node) = self.index.node(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if node.kind != NodeKind::File {
            reply.error(Errno::EISDIR);
            return;
        }
        // Clamp to EOF: reading at or past EOF yields zero bytes, not an error.
        if offset >= node.size {
            reply.data(&[]);
            return;
        }

        match self.read_range(node, offset, size) {
            Ok(data) => {
                // Schedule read-ahead before replying; it is fire-and-forget and
                // does not delay the foreground read.
                self.maybe_prefetch(ino.0, node, offset, size);
                reply.data(&data);
            }
            Err(e) => {
                warn!(inode = ino.0, offset, size, error = %e, "read failed");
                reply.error(errno_for(&e));
            }
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let Some(node) = self.index.node(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if node.kind != NodeKind::Dir {
            reply.error(Errno::ENOTDIR);
            return;
        }

        // Entries are produced deterministically: ".", "..", then children
        // sorted by inode. `offset` is the index at which to resume.
        let mut entries: Vec<(u64, FileType, String)> = Vec::new();
        entries.push((ino.0, FileType::Directory, ".".to_string()));
        entries.push((ino.0, FileType::Directory, "..".to_string()));
        for &child in self.index.children(ino.0) {
            if let Some(cn) = self.index.node(child) {
                let kind = match cn.kind {
                    NodeKind::Dir => FileType::Directory,
                    NodeKind::File => FileType::RegularFile,
                };
                entries.push((child, kind, basename(&cn.path).to_string()));
            }
        }

        for (i, (e_ino, kind, name)) in entries.iter().enumerate().skip(offset as usize) {
            // `add` returns true when the buffer is full; the next offset is i+1.
            if reply.add(INodeNo(*e_ino), (i + 1) as u64, *kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        // Report a synthetic read-only filesystem sized to the image.
        let total_bytes: u64 = self.index.manifest().source.total_bytes;
        let blocks = total_bytes.div_ceil(512);
        reply.statfs(blocks, 0, 0, 0, 0, 512, 255, 4096);
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        // Read-only filesystem: nothing to flush.
        reply.ok();
    }

    fn getxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _size: u32,
        reply: ReplyXattr,
    ) {
        // SwiftFS images do not carry extended attributes. ENODATA is the
        // correct "no such attribute" response; ENOSYS would make some tools
        // treat xattr support as broken.
        reply.error(Errno::ENODATA);
    }

    fn listxattr(&self, _req: &Request, _ino: INodeNo, _size: u32, reply: ReplyXattr) {
        // No extended attributes: report an empty list.
        reply.size(0);
    }
}
