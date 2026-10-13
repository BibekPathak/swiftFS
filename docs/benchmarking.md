# Benchmarking

## Overview

SwiftFS performance benchmarks compare lazy loading against eager extraction across
multiple dimensions. Results are stored as raw CSV/JSON and summary tables are
generated from raw data. All workloads are repeatable with warm-up runs and
controlled cache state.

## Comparisons

- **A.** Eager extraction to a normal filesystem (`cp -r` then local reads)
- **B.** SwiftFS with cold caches (no pre-loading)
- **C.** SwiftFS with warm caches (files previously read are in RAM cache)
- **D.** SwiftFS read-ahead disabled vs enabled
- **E.** SwiftFS across chunk sizes (512 KiB, 1 MiB, 4 MiB) and concurrency limits (1, 2, 4)

## Workloads

- **W1.** Many small files (100 files × 1 KiB each, random reads)
- **W2.** A few very large files (3 files × 10 MiB each, sequential reads)
- **W3.** Sparse access to a large image (100 MiB image with 1 KiB random reads)
- **W4.** Sequential reads (10 MiB file, contiguous chunk access)
- **W5.** Random reads (10 MiB file, random chunk access pattern)
- **W6.** Repeated hot-file access (same 2 MiB file read 20 times)
- **W7.** Concurrent reads from 3 processes (20 files each, overlapping access)
- **W8.** Model-artifact dataset (hot 2 MiB weights + 50 cold 1 KiB files)

## Metrics

The harness emits exactly these fields (per workload, across `--iterations`
runs); nothing is fabricated:

- `prep_ms` / `total_ms`   Preparation (build/inspect/verify/mount-load) and total wall-clock time
- `ttfa_ms`                Time to first chunk read
- `p50_ms`, `p99_ms`       Read latency percentiles (nearest-rank)
- `peak_rss`               Peak RSS in bytes, from `/proc/self/status` (`VmHWM`)
- `cache_hit_rate`         Fraction of reads served without a backend fetch (0..1)
- `storage_requests`       Backend fetches actually performed
- `bytes_fetched`          Bytes fetched from the backend
- `raw`                    Per-workload detail (sample counts, RAM hits, backend fetches, cold/warm comparison)

Metrics such as `throughput_mb_s` and `read_ahead_ratio` are **not** currently
produced; they are not listed as available.

## Results Storage

- Raw results as `bench_results/<dataset>_<config>.json`
- Summary tables as `bench_results/<dataset>_summary.csv`
- JSON structure includes: dataset metadata, build/inspect/verify times, CLI benchmark output, mount attempt info
- CSV structure: stage, time_s, rc, details

## Guardrails

- All workloads run N≥3 iterations; median reported
- Warm-up phase: first iteration discarded, caches pre-heated thereafter
- Machine/environment metadata recorded with each result set
- Mount time strictly separated from application startup latency
- File-access latency separated from model-load time
- No `strace`/`perf`/`iostat`/`/usr/bin/time` available in this environment;
  timings use `time` command and Python `time.time()` / `subprocess`

## Tooling

- `swiftfs bench <workload>` — run one workload with real measurements
- `swiftfs bench full` — run every workload and print a summary table
- `swiftfs-bench` — standalone harness over the same measurements
- `swiftfs build <source> <image> --chunk-size N` — build a SwiftFS image
- `swiftfs inspect <image>` / `swiftfs verify <image>
- `python3 bench_suite.py <source_dir>` — build an image, run the suite, attempt
  a real FUSE mount + full-tree read, and write JSON/CSV results
- `bench_scripts/gen_datasets.py` — generate synthetic datasets

## Example Commands

```bash
# Build an image and run every workload against it.
swiftfs build ./myroot ./myimage --chunk-size 1048576
swiftfs bench full --image ./myimage --data-dir ./myroot --iterations 5

# One workload.
swiftfs bench read --image ./myimage --iterations 5

# Compare chunk sizes for build/read cost.
swiftfs bench read --image ./myimage --iterations 5 --no-read-ahead
swiftfs bench read --image ./myimage --iterations 5

# Full orchestration with a real FUSE mount test and JSON/CSV output.
python3 bench_suite.py ./myroot --iterations 5
```

## How measurements are produced

`swiftfs bench` performs genuine work for every number it prints:

- `build`, `inspect`, `verify`: wall-clock time of the corresponding image
  operation, over `--iterations` runs (median via p50).
- `mount`: time to load + validate the manifest and build the immutable
  `ManifestIndex` — the work done before the FUSE session starts. It is **not**
  reported as application startup time.
- `read` (cold) / `warm_read` (warm) / `compare` / `read-ahead` / `io`: latency
  samples from real chunk reads through the same `ChunkFetcher` (RAM cache,
  disk cache, single-flight coalescing) used by the FUSE read path. Cold reads
  clear the RAM cache before each measured pass; warm reads reuse it. Cache-hit
  rate and backend-fetch counts come from the fetcher's own metrics, measured
  as deltas around the measured phase.
- p50/p99 use the nearest-rank method over the collected samples.
- Peak RSS is read from `/proc/self/status` (`VmHWM`) on Linux.

FUSE mounting is deliberately not attempted from inside the Rust harness, since
`/dev/fuse` availability varies by host. The Python `bench_suite.py` drives a
real mount when `/dev/fuse` and `fusermount` are present and reports a clear
message when they are not.

## Read-ahead

Sequential read-ahead is implemented in the daemon (`read_ahead.rs`, wired into
the FUSE `read` callback): a read of at least `READ_AHEAD_MIN_READ` bytes that
continues within `READ_AHEAD_WINDOW` bytes of the previous read on the same
inode triggers background prefetch of up to `READ_AHEAD_CHUNKS` following
chunks. Prefetches use the same coalescing fetcher and run on the daemon's
Tokio runtime; they never block the FUSE worker thread. `--no-read-ahead`
disables the heuristic.

## Status

No performance numbers are asserted in this document. Run `swiftfs bench full`
or `python3 bench_suite.py` to produce results for your machine and dataset;
they are written under the work directory (JSON + CSV). This keeps every figure
traceable to a command, a dataset, a cache state, and a machine.