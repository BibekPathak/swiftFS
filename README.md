# SwiftFS

A read-only, lazy-loading container filesystem in Rust.

SwiftFS exposes the contents of an image through a mounted filesystem while
fetching file data only when it is actually accessed. This is intended to reduce
time to first process execution and time to first inference for large container
images and ML model artifacts, compared with eagerly downloading and extracting
everything up front.

> **Status: feature-complete for the documented scope.** `swiftfs build`,
> `inspect`, `verify`, `mount`, `unmount`, and `bench` all work. A mounted image
> lists directories without fetching file data and reads bytes on demand,
> verified against BLAKE3 chunk hashes. Storage backends (local directory and
> HTTP byte-range), bounded RAM + optional disk caches, single-flight request
> coalescing, a Prometheus metrics endpoint, sequential read-ahead, and a
> config file are all reachable from the CLI. `swiftfs bench` reports **real**
> measurements (it does not fabricate numbers); see
> [`docs/benchmarking.md`](docs/benchmarking.md).

## Documentation

| Document | Contents |
| --- | --- |
| [`docs/architecture.md`](docs/architecture.md) | Component layout and data flow |
| [`docs/design-decisions.md`](docs/design-decisions.md) | Decisions, trade-offs, threading model |
| [`docs/image-format.md`](docs/image-format.md) | Manifest schema and chunking |
| [`docs/benchmarking.md`](docs/benchmarking.md) | Benchmark methodology and results |
| [`docs/troubleshooting.md`](docs/troubleshooting.md) | Common setup issues |

## Environment requirements

- Linux (FUSE support: `/dev/fuse`, `fuse3`).
- Stable Rust (see `rust-toolchain.toml`).
- Docker (for reproducible integration/benchmark environments).
- Optional: `strace`, `perf`, `iostat` for profiling.

This repository was initialized in an environment with Rust 1.93.1, `fuse3`
3.18.2, and Docker 29.7.2. `strace`, `perf`, and `iostat` were **not** available,
which is recorded in the eventual benchmark report rather than assumed.

## Building

```sh
cargo build --workspace
cargo test  --workspace
cargo clippy --workspace --all-targets
cargo fmt --all --check
```

## CLI

The `swiftfs` binary is scaffolded with the intended subcommands:

```
swiftfs build   <source> <output> [--chunk-size BYTES]
swiftfs inspect <image>
swiftfs mount   <image> <target> [options]
swiftfs unmount <target>
swiftfs verify  <image>
swiftfs bench   [workload] [--image IMAGE] [--data-dir DIR] [--iterations N]
```

All subcommands are implemented. `mount` options:

```
--allow-other                 expose the mount to other users
--auto-unmount                unmount when the mounting process exits
--threads N                   FUSE worker threads (default 4)
--http-base-url URL           fetch chunks over HTTP instead of locally
--disk-cache-dir DIR          enable a persistent content-addressed disk cache
--disk-cache-bytes N          disk cache capacity (default 1 GiB)
--ram-cache-bytes N           RAM cache capacity (default 256 MiB)
--concurrent-fetches N        max outstanding backend fetches (default 32)
--storage-timeout-ms N        per-request storage timeout (default 15000)
--no-read-ahead               disable sequential read-ahead
--metrics-listen HOST:PORT    serve Prometheus metrics at /metrics
--config FILE                 load settings from a YAML file (flags override)
```

`bench` reports real measurements; see
[`docs/benchmarking.md`](docs/benchmarking.md).

### End-to-end example (Phase 2 + 3)

```sh
# Create some content and build an image.
mkdir -p /tmp/myroot/sub
echo "hello" > /tmp/myroot/hello.txt
head -c 2097153 /dev/urandom > /tmp/myroot/sub/big.bin

swiftfs build   /tmp/myroot /tmp/myimage --chunk-size 1048576
swiftfs inspect /tmp/myimage
swiftfs verify  /tmp/myimage

# Mount read-only and use it like a normal directory tree.
mkdir -p /tmp/myimage-mnt
swiftfs mount /tmp/myimage /tmp/myimage-mnt

ls -la /tmp/myimage-mnt
cat /tmp/myimage-mnt/hello.txt
sha256sum /tmp/myimage-mnt/sub/big.bin      # matches the source file

swiftfs unmount /tmp/myimage-mnt
```

`build` writes `manifest.json` and content-addressed chunk objects under
`chunks/`, deduplicating identical chunks and rejecting symlinks and special
files. It refuses to clobber a non-empty output directory.

`mount` loads only the manifest at mount time; file contents are fetched and
hash-verified on first read. The mount is read-only, so writes, creates, and
other mutations fail with `EROFS`.

## License

MIT OR Apache-2.0.
