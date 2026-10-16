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
| `common.py` | Shared helpers: process/RSS/CPU sampling, metrics scraping, statistics |
| `run_matrix.sh` | Driver for the chunk-size × access-pattern matrix |
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
    --chunk-size 65536 --metrics --validate
```

`--metrics` enables the daemon `/metrics` endpoint so the harness can measure
**actual** backend bytes (`swiftfs_backend_bytes_total`) and fetches; without it
those fields are reported as unknown (`null`), never a fabricated zero.
`--validate` mounts the image and checks that eager and FUSE return byte-identical
data for the workload.

Run the full chunk-size × access-pattern matrix (writes raw results to
git-ignored `bench-out/`):

```sh
DS=/tmp/ml_large OUT=$PWD/bench-out bash -c '
  source bench/run_matrix.sh
  run_cell 1048576 sequential 4 131072 1m_seq
  run_cell  65536 sequential 4 131072 64k_seq
  run_cell  16384 sequential 4 131072 16k_seq
'
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
  - `fuse_cold` — fresh SwiftFS mount (empty SwiftFS caches), then read.
  - `fuse_warm` — the same mount re-read (caches populated by the cold pass);
    this is a *repeat read*, not a cold-throughput peer.
- **Statistics**: N iterations (default 5); the median is reported with min/max
  spread. Cold and warm are explicitly separated rather than one being discarded.
- **Backend traffic** is measured from the daemon's `/metrics` endpoint when
  `--metrics` is given, as a delta around each phase:
  - `backend_fetches` (`swiftfs_backend_fetches_total`) — fetches after coalescing.
  - `backend_bytes_actual` (`swiftfs_backend_bytes_total`) — **true** bytes moved.
  - `backend_bytes_nominal` — `fetches × chunk_size`, an **upper bound** kept only
    for comparison; it overstates short/tail chunks.
  - `amplification_actual = backend_bytes_actual / app_bytes`.
  When `--metrics` is off, these are reported as **unknown (`null`)**, never `0`.
  For `eager` they are undefined: a plain copy has no backend, so eager is never
  reported as zero backend traffic.
- **Equivalence** (`--validate`): every requested range is compared byte-for-byte
  between the source tree and the mount, with a BLAKE2b checksum.
- **Timing boundary**: eager `end_to_end_s = prep + read`; FUSE
  `end_to_end_s = mount + read`.
- **CPU** (`cpu_s`) is sampled from `/proc/<pid>/stat`; **peak RSS** from
  `/proc/<pid>/status` (`VmHWM`) of the mount process.

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
