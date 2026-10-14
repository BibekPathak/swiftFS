# SwiftFS benchmark harness

Reproducible, dependency-free (Python standard library) benchmarks comparing
**eager extraction** with **SwiftFS lazy loading** through a real FUSE mount.

These tools do not fabricate numbers: every figure is produced by running real
work and is written to machine-readable output. Raw outputs and generated
datasets are **not** committed; only compact summaries belong in `docs/`.

## Files

| File | Purpose |
| --- | --- |
| `dataset.py` | Deterministic, configurable synthetic dataset generator + manifest |
| `compare.py` | Eager-vs-lazy comparison harness (eager, fuse_cold, fuse_warm) |
| `common.py` | Shared helpers: process/RSS sampling, environment manifest, statistics |
| `datasets/*.json` | Named dataset configurations |

## Quick start

```sh
# 1. Build the release binary.
cargo build --release --workspace

# 2. Generate a deterministic dataset (refuses to overwrite an existing dir).
python3 bench/dataset.py /tmp/sfs_ds --config bench/datasets/ml-mixed.json

# 3. Compare eager vs SwiftFS cold/warm for a defined workload.
python3 bench/compare.py /tmp/sfs_ds \
    --out /tmp/sfs_cmp \
    --iterations 5 \
    --access sequential --reads-per-file 1 --read-bytes 131072 \
    --metrics
```

Outputs under `--out`:

- `results.json` — raw per-iteration runs plus medians and spread.
- `summary.csv` — compact median table.
- `environment.json` — machine manifest (CPU, kernel, filesystems, tools, config).
- `mount_iterN.log` — per-iteration mount logs (diagnostics).

## Methodology

- **Equivalent useful work**: every configuration executes the *same* list of
  byte-range reads generated from the dataset manifest and the access pattern.
- **Method tagging**: each result row is tagged `eager`, `fuse_cold`, or
  `fuse_warm`. Numbers from different methods are never placed in one column's
  comparison without the tag.
- **Configurations**:
  - `eager` — dataset `cp -r`'d to a local directory, then read.
  - `fuse_cold` — fresh SwiftFS mount (empty RAM cache), then read.
  - `fuse_warm` — the same mount re-read (RAM cache populated by the cold pass).
- **Statistics**: N iterations (default 5); the median is reported with min/max
  spread. The first pass is not discarded automatically; instead cold and warm
  are explicitly separated.
- **Backend fetches** (`bytes_fetched`) are measured from the daemon's
  `/metrics` endpoint when `--metrics` is given, as a delta around each phase.
  When disabled, the value is reported as **unknown (blank)**, never `0`.
- **Peak RSS** comes from `/proc/<pid>/status` (`VmHWM`) of the mount process.

## Environment caveats (read these)

- If the dataset/image live on `tmpfs`, absolute read latencies reflect memory
  copy, not disk I/O. Record your filesystem in `environment.json`.
- The OS page cache cannot generally be dropped unprivileged, so `eager` reads
  are usually warm. Cold/warm here refers to **SwiftFS** caches.
- `perf`/`strace`/`iostat` are not required; the harness works without them.

## FUSE-less fallback

If `/dev/fuse` or `fusermount` is unavailable, `eager` still runs and the
`fuse_cold`/`fuse_warm` rows are reported as `skipped` with an explicit reason.
Use `--force-mount` only to attempt a mount despite the detection check.
