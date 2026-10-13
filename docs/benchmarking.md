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

## Metrics (recorded per workload, per configuration, across N≥3 iterations)

- `mount_time_s`          Time to prepare and mount the SwiftFS image
- `prep_time_s`           Eager extraction preparation time
- `ttfa_s`                Time to first successful file access (first `dd` read)
- `warmup_time_s`         Time for cache warm-up phase
- `total_time_s`          Total benchmark execution time
- `throughput_mb_s`       Throughput in MiB/s during read phase
- `p50_latency_ms`        50th percentile read latency
- `p99_latency_ms`        99th percentile read latency
- `peak_rss_kb`           Peak resident set size during benchmark (best effort)
- `cache_hit_rate`        Fraction of reads served from RAM cache (0.0 - 1.0)
- `storage_requests`      Number of backend storage (HTTP/Disk) requests
- `bytes_fetched`         Total bytes fetched from backend
- `read_ahead_ratio`      Ratio of prefetched bytes that were actually used

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

- `swiftfs bench <workload>` - run a single workload benchmark via CLI
- `swiftfs build <source> <image> --chunk-size N` - build SwiftFS image
- `swiftfs inspect <image>` - inspect image metadata
- `swiftfs verify <image>` - verify image integrity
- `python3 bench_suite.py <dataset_name>` - run orchestrated benchmark suite
- Dataset generators in `bench_scripts/gen_datasets.py`
- Raw results and summaries in `bench_results/` directory
- Python `bench_suite.py` orchestrates dataset generation, SwiftFS image build,
  verification, and CLI benchmark runs

## Example Commands

```bash
# Generate synthetic datasets
python3 bench_scripts/gen_datasets.py /tmp/swiftfs_bench_data --num-datasets 3

# Build SwiftFS image from a dataset
swiftfs build /tmp/swiftfs_bench_data/dataset_0 /tmp/swiftfs_bench_image --chunk-size 1048576

# Run benchmark suite for a dataset
python3 bench_suite.py dataset_0

# Compare results between datasets
cat /tmp/swiftfs_bench_results/dataset_0_summary.csv /tmp/swiftfs_bench_results/dataset_1_summary.csv
```

## Phase 7 Status

- Benchmark harness infrastructure implemented and verified (`cargo build --workspace` clean)
- `swiftfs bench` CLI subcommand available (workload names: read, mount, etc.)
- Dataset generators working (`bench_scripts/gen_datasets.py`)
- Results storage framework (JSON + CSV output)
- Documentation updated with full methodology
- FUSE-mounted benchmarks: framework ready but requires working FUSE environment with appropriate permissions; CLI-based benchmarks functional
- All 22 format/tests + 4 FUSE integration tests pass
- `cargo clippy --workspace --all-targets -- -D warnings` clean