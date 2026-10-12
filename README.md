# SwiftFS

A read-only, lazy-loading container filesystem in Rust.

SwiftFS exposes the contents of an image through a mounted filesystem while
fetching file data only when it is actually accessed. This is intended to reduce
time to first process execution and time to first inference for large container
images and ML model artifacts, compared with eagerly downloading and extracting
everything up front.

> **Status: Phase 1 (foundation) complete.** The workspace builds and the design
> is documented. The image format, FUSE mount, storage backends, caching, and
> benchmarks are implemented in later phases. This README does **not** yet claim
> working end-to-end behavior.

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

Subcommands currently report that they are implemented in a later phase.

## License

MIT OR Apache-2.0.
