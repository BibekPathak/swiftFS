# Design decisions

This document records the decisions made in Phase 1 and the reasoning behind
them. Later phases append decisions as they are made.

## D1. FUSE library: `fuser` 0.18

`fuser` is the most widely used and actively maintained Rust FUSE binding and
links against system `libfuse3`, which is present in this environment
(`fuse3` 3.18.2). Pinned to `0.18.0`.

Verified from the crate source (`fuser-0.18.0/src/lib.rs`):

- `pub trait Filesystem: Send + Sync + 'static` — a **synchronous** trait.
- Callbacks receive a `reply` handle and are expected to be answered inline:

  ```rust
  fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry);
  fn getattr(&self, req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr);
  fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen);
  fn read(&self, req: &Request, ino: INodeNo, fh: FileHandle, offset: u64,
          size: u32, flags: OpenFlags, lock_owner: Option<LockOwner>, reply: ReplyData);
  fn release(&self, req: &Request, ino: INodeNo, fh: FileHandle, flags: OpenFlags,
             lock_owner: Option<LockOwner>, flush: bool, reply: ReplyEmpty);
  fn readdir(&self, req: &Request, ino: INodeNo, fh: FileHandle, offset: i64,
             reply: ReplyDirectory);
  fn init(&mut self, req: &Request, config: &mut KernelConfig) -> io::Result<()>;
  fn destroy(&mut self);
  ```

  (Types are in `fuser::ll` as of 0.18; `INodeNo`, `FileHandle`, `Errno` are
  newtypes rather than bare integers.)

## D2. Bridging synchronous FUSE callbacks to async storage

**Decision:** run a dedicated Tokio multi-threaded runtime for storage, HTTP,
and cache work. FUSE callbacks run on `fuser`'s own synchronous worker threads.
When a callback needs data, it `block_on`s (or blocks on a channel) tied to the
Tokio runtime's **blocking pool**, so it waits on storage without occupying a
Tokio worker thread.

Rationale:

- `fuser`'s trait is synchronous; callbacks cannot `.await`. The library's
  `experimental::AsyncFilesystem` adapter exists, but it runs the async future
  to completion on the current thread via `block_on` and does not give us a
  shared multi-thread runtime with backpressure control. Using the synchronous
  `Filesystem` directly and owning the bridge keeps the concurrency model
  explicit and testable.
- Tokio worker threads must never block on synchronous I/O. All waits happen on
  `fuser` threads or the Tokio blocking pool, never on a worker.

**Consequences / risks:**

- Each blocked FUSE worker occupies a thread while a fetch is in flight. This is
  bounded by configuring `fuser`'s session thread count and by bounding
  concurrent fetches.
- Deadlock risk is contained by never awaiting a Tokio worker from within a
  FUSE callback and by using single-flight maps that clean up on both success
  and failure. This is revisited and documented further in Phase 4/5.

## D3. Workspace over a single crate

A Cargo workspace with one crate per concern (format, storage, cache, daemon,
bench, core) keeps dependencies acyclic and modules cohesive. `swiftfs-core`
holds only shared primitives. This is slightly more boilerplate than a single
crate but makes the security- and concurrency-critical boundaries explicit.

## D4. Chunking and deduplication

Default chunk size is 1 MiB (`DEFAULT_CHUNK_SIZE`). Chunks are content-addressed
by BLAKE3 hash, enabling deduplication and integrity verification. Chunk size is
configurable because it directly trades metadata size against fetch granularity
and is a first-class benchmark dimension.

## D5. Read-only by construction

The filesystem implements only read-side operations. Unsupported operations
(`write`, `create`, `unlink`, `mkdir`, symlinks, special files) are left to the
trait defaults, which reply `ENOSYS`/appropriate errors. The README documents
what is deliberately unsupported rather than implying write support.

## D6. HTTP backend without a cloud account

The HTTP backend performs byte-range GETs against any HTTP server. Tests use a
local mock server; no paid cloud account is required. Documented server
requirements: support for `Range` requests (`206 Partial Content`) and correct
`Content-Length` on partial responses. Backends fall back to whole-object fetch
only if range support is provably absent, and this is documented, not silent.

## D7. Security boundary

Malformed manifests and untrusted storage are treated as hostile. Validation
covers path traversal, path/inode collisions, integer overflow, chunk tiling,
and buffer sizing. Fetched chunks are hash-verified before entering a cache. The
disk cache uses atomic publication (temp file + rename). `#![forbid(unsafe_code)]`
is set on format, storage, and cache crates; any `unsafe` required by the FUSE
layer is confined to `swiftfs-daemon` with a documented safety argument, or
avoided entirely.

SwiftFS is a read-only image filesystem, **not** a container sandbox and not a
replacement for a secure container runtime.

## D8. Tooling constraints observed in this environment

- Available: Rust 1.93.1, `fuse3` 3.18.2, Docker 29.7.2, `cargo-nextest`.
- Not available: `strace`, `perf`, `iostat`, `/usr/bin/time`.

Benchmark and profiling reports will state which tools were actually used and
which were unavailable, rather than implying profiling that did not happen.
