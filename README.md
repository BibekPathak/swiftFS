# SwiftFS

A read-only, lazy-loading container filesystem in Rust.

SwiftFS exposes the contents of an image through a mounted filesystem while
fetching file data only when it is actually accessed. This is intended to reduce
time to first process execution and time to first inference for large container
images and ML model artifacts, compared with eagerly downloading and extracting
everything up front.

> **Status: Phase 2 (image format and builder) complete.** The `swiftfs build`,
> `inspect`, and `verify` commands work against the local image layout, with
> tests covering chunking, deduplication, byte-range correctness, and corruption
> detection. The FUSE mount, storage backends, caching, and benchmarks are
> implemented in later phases. `mount`/`unmount`, the HTTP backend, and
> benchmarks are **not** yet functional.

## Documentation

| Document | Contents |
| --- | --- |
| [`docs/architecture.md`](docs/architecture.md) | Component layout and data flow |
| [`docs/design-decisions.md`](docs/design-decisions.md) | Decisions, trade-offs, threading model |
| [`docs/image-format.md`](docs/image-format.md) | Manifest schema and chunking |
| [`docs/benchmarking.md`](docs/benchmarking.md) | Benchmark methodology (Phase 6) |
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
swiftfs mount   <image> <target>
swiftfs unmount <target>
swiftfs verify  <image>
swiftfs bench   [workload]
```

Subcommands `build`, `inspect`, and `verify` are implemented. `mount`,
`unmount`, and `bench` report that they are implemented in a later phase.

### End-to-end example (Phase 2)

```sh
# Create some content and build an image.
mkdir -p /tmp/myroot/sub
echo "hello" > /tmp/myroot/hello.txt
head -c 2097153 /dev/urandom > /tmp/myroot/sub/big.bin

swiftfs build /tmp/myroot /tmp/myimage --chunk-size 1048576
swiftfs inspect /tmp/myimage
swiftfs verify  /tmp/myimage
```

`build` writes `manifest.json` and content-addressed chunk objects under
`chunks/`, deduplicating identical chunks and rejecting symlinks and special
files. It refuses to clobber a non-empty output directory.

## License

MIT OR Apache-2.0.
