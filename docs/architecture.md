# Architecture

SwiftFS is a read-only FUSE filesystem that serves file contents from a
versioned image composed of a manifest plus independently addressable chunks.

## Goals

- Load compact metadata at mount time; do not read file data until accessed.
- Fetch only the chunks needed to satisfy a read.
- Support local and HTTP-backed chunk storage.
- Bound memory and concurrency; coalesce duplicate in-flight fetches.
- Provide reproducible benchmarks against eager extraction.

## Non-goals

- Writable filesystems, hard links, symlinks, sparse files, or special files.
- Acting as a container sandbox or a substitute for a secure container runtime.

## Crate layout

```
crates/
├── swiftfs-core/     Shared primitives: errors, constants, root inode.
├── swiftfs-format/   Manifest schema, builder, validation, chunking.
├── swiftfs-storage/  Backend-agnostic chunk fetch (local, HTTP).
├── swiftfs-cache/    Bounded multi-level cache + single-flight coalescing.
├── swiftfs-daemon/   Read-only FUSE filesystem + `swiftfs` CLI.
└── swiftfs-bench/    Benchmark/workload harness.
```

Dependencies flow in one direction:

```
core <- format
core <- storage
core <- cache
format, storage, cache <- daemon
format, storage, cache <- bench
```

`core` deliberately holds only primitives shared by multiple crates so that no
cycles form between format, storage, and cache.

## Component responsibilities

### swiftfs-format

Defines the immutable, versioned manifest: paths, POSIX metadata, stable inode
IDs, chunk references (offset/length/hash/object id), and optional compression
metadata. Provides the builder that traverses a directory, chunks regular files,
hashes and deduplicates chunks, and writes the image atomically. Provides
validation for untrusted manifests. See [`image-format.md`](image-format.md).

### swiftfs-storage

A `ChunkLocation` describes where a chunk lives (object key, offset, length).
Backends implement fetching those ranges. Phase 4 adds a local directory backend
and an HTTP byte-range backend with validation, timeouts, bounded retries, and
hash verification.

### swiftfs-cache

A capacity-bounded RAM cache, an optional atomic disk cache, and single-flight
coalescing so that concurrent requests for the same missing chunk share one
fetch. Phase 4.

### swiftfs-daemon

The FUSE filesystem implementing `lookup`, `getattr`, `readdir`, `open`, `read`,
`release`, `init`, and `destroy`. Also hosts the `swiftfs` CLI.

### swiftfs-bench

Reproducible synthetic and model-shard-style workloads comparing eager
extraction with SwiftFS under cold/warm caches and varied tuning. Phase 6.

## Data flow (target end state)

```
build:   source dir -> traversal -> chunk split -> hash/dedupe -> manifest + chunk objects

mount:   manifest -> in-memory tree (no file data)
read:    FUSE read(ino, offset, size)
           -> chunk range mapping
           -> cache lookup (RAM, then disk)
           -> miss: single-flight fetch -> storage backend
           -> verify BLAKE3 -> publish into cache
           -> serve bytes; optionally schedule read-ahead
```

## Threading overview

`fuser` 0.18 exposes a **synchronous** `Filesystem` trait (`Send + Sync +
'static`) whose methods receive a `reply` handle. SwiftFS runs storage work on a
dedicated Tokio multi-threaded runtime and bridges it to the synchronous FUSE
callbacks by blocking the FUSE worker thread on a runtime handle, never by
blocking a Tokio worker thread. Full details and safety reasoning are in
[`design-decisions.md`](design-decisions.md).
