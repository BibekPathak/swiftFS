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

The HTTP backend performs byte-range GETs against any HTTP server. Integration
tests run against a local mock server implemented on a `TcpListener` (no
external dependency, loopback only); no paid cloud account is required.
Documented server requirements: support for `Range` requests (`206 Partial
Content`) and correct `Content-Length` on partial responses. If a server ignores
`Range` and returns `200 OK`, the response is rejected unless the body length
exactly matches the requested chunk — SwiftFS will not silently download a
whole (possibly huge) object. The `Content-Range` header, when present, is
validated against the requested byte span.

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

## D9. Format v1 chunk-object layout (Phase 2)

In format v1 each chunk object holds exactly one chunk, named by its BLAKE3 hex
hash, so `object == hash` and `object_offset == 0`. Deduplication reuses the
object file for identical content. This is the simplest correct layout and makes
`verify` trivially hash whole files. The `object`/`object_offset` schema fields
are retained so a later version can pack many chunks into one object without a
format-version break for readers that already understand the fields.

## D10. Atomic build and non-clobbering output (Phase 2)

`build` writes into a per-process staging directory (`.{name}.staging.{pid}`)
next to the target, fsyncs each object, then renames the staging directory into
place. The manifest itself is written to `manifest.json.tmp` and renamed after
`sync_all`. A crash therefore cannot leave a valid-looking partial image. The
builder refuses a non-empty existing output directory rather than merging into or
overwriting it, and cleans up staging on any error. A cross-device rename
fallback copies the tree and removes staging.

## D11. Rejecting unsupported inputs explicitly (Phase 2)

Traversal uses `symlink_metadata` and does not follow symlinks; symlinks and
special files are rejected with `SwiftfsError::UnsupportedFileType` rather than
silently skipped or mis-stored. Manifest validation rejects unknown format
versions, bad paths (traversal, trailing slash, `.` components), duplicate
inodes/paths, malformed hashes, and inconsistent chunk tiling, and bounds
`chunk_size` to `1..=64 MiB` so validation never allocates from an
attacker-controlled size.

## D12. Immutable in-memory index, no locks on the read path (Phase 3)

`ManifestIndex` is built once at mount time from the validated manifest and is
never mutated. It holds `HashMap`s for inode→node, (parent, name)→inode, and
inode→children, plus sorted child lists for stable `readdir`. Because it is
immutable, it is shared behind `Arc` with no lock, so no lock is ever held across
a `read` or a storage fetch. Mount-time work is limited to reading and indexing
the manifest; no file contents are fetched.

## D13. Synchronous FUSE callbacks bridged to Tokio via oneshot (Phase 3)

For `read`, the FUSE worker thread spawns a fetch task on the `swiftfs-io` Tokio
runtime and blocks on a `tokio::sync::oneshot` receiver. The runtime worker that
executes the fetch is never blocked. The FUSE session is configured with
`n_threads` worker threads so several reads can be outstanding at once, each
blocking only its own FUSE thread. This is deliberately *not* the
`experimental::AsyncFilesystem` adapter, which runs futures on the calling
thread and would give up the disjoint thread pools. Deadlock risk is avoided by
never awaiting runtime work from inside a callback and by not holding locks
across the bridge.

## D14. EOF, errors, and read-only semantics (Phase 3)

- Reading at or past EOF returns zero bytes, not an error; a range that starts
  before EOF and extends past it returns the tail.
- Writes and creates are rejected with `EROFS`; `open` rejects non-read modes
  before any storage work.
- A missing object maps to `ENOENT`; a truncated or hash-mismatched object maps
  to `EIO`; unsupported operations map to `EINVAL`.
- Chunk bytes are hash-verified in the `read` path before being served, in
  addition to `verify` at rest.
- `getxattr` returns `ENODATA` and `listxattr` reports an empty list, so tools do
  not treat xattr support as broken.

## D15. Local backend reads on the blocking pool (Phase 3)

`LocalStore` performs synchronous file reads but runs them via
`tokio::task::spawn_blocking`, so it is safe on both current-thread and
multi-thread runtimes and never blocks a runtime worker. This replaced an
initial `block_in_place`, which panics on current-thread runtimes (found via a
test failure).

## D16. Open fds block unmount (Phase 3)

During integration testing, unmounting while a file handle was still open caused
the unmount to block. Tests close their handles before unmounting, and this is
documented as expected FUSE behavior rather than worked around. `swiftfs
unmount` shells out to `fusermount3` (falling back to `umount`), which handles
the normal case.

## D17. Sequential read-ahead on the read path

The FUSE `read` callback, after serving the requested bytes, consults a small
per-inode tracker (`read_ahead.rs`). A read of at least `READ_AHEAD_MIN_READ`
bytes that continues within `READ_AHEAD_WINDOW` bytes of the previous read on
the same inode is treated as sequential and triggers a background prefetch of up
to `READ_AHEAD_CHUNKS` following chunks.

Rationale and properties:

- The image is immutable, so prefetching can only change which chunks are
  resident, never what is returned. The read-ahead result is therefore
  correctness-neutral; `read_ahead_returns_correct_bytes_on_and_off` asserts
  this through a real mount.
- Prefetches use the same `ChunkFetcher` as foreground reads, so they are
  coalesced: a prefetch and a real read for the same chunk share one backend
  fetch. This is why enabling read-ahead does not increase backend request
  counts, only cache-hit rates.
- Prefetch tasks run on the daemon's Tokio runtime and never block the FUSE
  worker thread that issued the read.
- The tracker is bounded (`MAX_TRACKED_INODES`) so adversarial access cannot grow
  it without limit; exceeding the bound clears the map, costing at most one cold
  read.
- `--no-read-ahead` (mount config `read_ahead`) disables the heuristic entirely.

## D18. Benchmarks measure real work and are reported as such

`run_benchmark` performs genuine operations: it times image `build`/`inspect`/
`verify`, times manifest load + index construction for `mount`, and collects
real chunk-read latency samples through the same fetcher the FUSE path uses.
Cold and warm cache states are controlled explicitly; cache-hit rate and backend
fetch counts are taken as deltas around the measured phase from the fetcher's
own metrics. p50/p99 use nearest-rank. Peak RSS comes from `/proc/self/status`.

The Rust harness deliberately does not attempt a FUSE mount (host-dependent), so
read latency is measured at the fetcher layer, which dominates a SwiftFS read.
`bench_suite.py` drives a real mount when `/dev/fuse` is available and reports a
clear message otherwise.

## D19. `swiftfs-bench` delegates to the daemon benchmark module

Rather than duplicate measurement logic, `swiftfs-bench` depends on
`swiftfs-daemon` and calls the same `run_benchmark`/`run_full_suite`. The main
`swiftfs bench` subcommand and the standalone binary therefore cannot drift.

## D20. Storage backends and caches are selected at mount time

`MountConfig` carries a `Backend` (`Local` or `Http { base_url }`), an optional
`DiskCacheConfig`, a RAM-cache byte budget, a fetch concurrency bound, and a
storage timeout. `mount` exposes all of these as flags (`--http-base-url`,
`--disk-cache-dir`, `--disk-cache-bytes`, `--ram-cache-bytes`,
`--concurrent-fetches`, `--storage-timeout-ms`). Previously the HTTP backend and
disk cache existed as tested library code but the daemon hardcoded `LocalStore`
and `None`; that gap is closed, and both paths are exercised by tests and
manual verification.

## D21. Optional Prometheus metrics endpoint

`--metrics-listen HOST:PORT` starts a small thread serving `GET /metrics` in
Prometheus text format. The body is rendered from the live `ChunkFetcher`
counters and the chunk-latency histogram — the same metrics the read path
updates. The server is a plain `TcpListener` (no async runtime, no new
dependencies), read-only, and stops when the mount ends. Metrics are also
printed by `swiftfs bench`.

## D22. Config file and `--allow-other` correction

`--config FILE` loads a flat `key: value` YAML-subset (no full YAML dependency;
unknown keys are ignored with a warning). Explicit flags override file values,
so a config file sets defaults and the command line can refine them.

The `--allow-other` flag previously pushed `MountOption::AutoUnmount` instead of
an allow-other option, and `auto_unmount` was applied unconditionally. This is
corrected: `--allow-other` adds the `allow_other` mount option, `--auto-unmount`
independently controls `AutoUnmount`, and the session ACL is `All` only when
`allow_other` is set. (`fuser` 0.18 dropped typed `AllowOther`/`AllowRoot`
variants, so the option is passed via `MountOption::CUSTOM("allow_other")`.)
