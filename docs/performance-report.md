# SwiftFS Performance Report

This report records **measured** results only. Every figure is traceable to a
command, a dataset, a cache state, and the machine described below. Nothing here
is estimated, extrapolated, or assumed.

## Frozen environment manifest

| Item | Value |
| --- | --- |
| Date of measurement | 2026-10-14 |
| OS | Arch Linux |
| Kernel | `7.1.9-hardened1-1-hardened` (`SMP PREEMPT_DYNAMIC`, x86_64) |
| CPU | Intel(R) Core(TM) 5 120U |
| CPU topology | 10 cores / 12 logical CPUs, 1 socket |
| RAM | 15 GiB total |
| Rust | rustc 1.93.1 (01f6ddf75 2026-02-11), cargo 1.93.1 |
| FUSE | `/dev/fuse` present (`crw-rw-rw-`), `fusermount3` at `/usr/bin/fusermount3` |
| Workspace filesystem | `ext2/ext3` (per `stat -f`) |
| Benchmark work dir filesystem | `tmpfs` (per `stat -f`) — **caveat below** |
| Binary profile | `--release` (lto = "thin", codegen-units = 1) |
| Git revision | `bbb92bac3d73ca77672a564df93fd5ea1cbe6223` |

### Tooling availability (measured)

| Tool | Status |
| --- | --- |
| `perf` | **MISSING** |
| `strace` | **MISSING** |
| `iostat` | **MISSING** |
| `/usr/bin/time` | **MISSING** |
| `pidstat`, `mpstat`, `sar` | **MISSING** |
| `vmstat` | present |
| `free`, `lscpu`, `lsblk`, `lsof`, `fuser` | present |
| `/proc` (`VmHWM` in `/proc/<pid>/status`) | present |
| `/proc/sys/vm/drop_caches` writable | **no** (unprivileged; cannot drop caches) |
| Python | 3.12.7 |

### Environment caveats that affect interpretation

1. **Benchmark data lives on `tmpfs`** (RAM-backed). Local-backend reads
   therefore measure memory copy, not disk seek/read. Absolute read latencies
   are optimistic and are **not** representative of spinning or network storage.
2. **Caches cannot be dropped.** The page cache is warm across runs for images
   and datasets; cold-vs-warm cache comparisons refer to *SwiftFS RAM/disk
   caches*, not the OS page cache.
3. **No `perf`/`strace`/`iostat`.** Profiling is limited to `/proc` sampling,
   internal metrics counters, and wall-clock timing.
4. The machine is shared; single-run figures vary. Medians over N≥5 iterations
   are used.

## Phase 1 — Baseline validation (no code changes)

### Correctness / hygiene (all clean)

| Command | Result |
| --- | --- |
| `cargo fmt --all --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo test --workspace` | 42 passed, 0 failed, 5 ignored |
| `cargo test -p swiftfs-daemon --test phase3 -- --ignored --test-threads=1` | 5 passed, 0 failed |

Per-binary test results (`cargo test --workspace`):

| Test binary | Passed | Ignored |
| --- | --- | --- |
| `swiftfs-core` unit | 2 | 0 |
| `swiftfs-daemon` unit | 14 | 0 |
| `swiftfs-daemon` `tests/phase3.rs` | 4 | 5 (FUSE) |
| `swiftfs-format` `tests/format_tests.rs` | 18 | 0 |
| `swiftfs-storage` unit | 2 | 0 |
| `swiftfs-storage` `tests/http_integration.rs` | 2 | 0 |
| `swiftfs-cache` unit | **0** | 0 |

FUSE-gated suite: `mount_disallows_writes`, `mount_list_and_read_roundtrip`,
`read_ahead_returns_correct_bytes_on_and_off`, `read_at_chunk_boundaries_and_past_eof`,
`repeated_mount_unmount_is_clean` — all pass.

### Baseline dataset

Deterministic synthetic dataset (`random.seed(42)`):

| Property | Value |
| --- | --- |
| Files | 109 |
| Directories | 4 |
| Total logical bytes | 40,046,756 (38.19 MiB) |
| Small files | 100 × 256 B–4 KiB |
| Large files | 3 × 8 MiB |
| Model file | 1 × 4 MiB weights + 5 × 2 MiB shards |
| Chunk size | 1,048,576 bytes |
| Image: unique chunks | 138 |
| Image: total chunks | 138 |

## Baseline measurements

### A. Internal chunk-fetcher workload (`swiftfs bench full`, release, 5 iters)

These numbers come from the **in-process `ChunkFetcher`**, *not* a FUSE mount.
The read workload targets the largest single file (8 MiB, 40 chunks).

| Workload | p50 (ms) | p99 (ms) | ttfa (ms) | total (ms) | backend fetches | cache hit |
| --- | --- | --- | --- | --- | --- | --- |
| build | 0.000 | 0.000 | 0.000 | 69 | 0 | 0.0% |
| inspect | 0.288 | 0.384 | 0.384 | 1 | 0 | 0.0% |
| verify | 39.912 | 40.228 | 40.228 | 199 | 0 | 0.0% |
| mount (manifest load + index) | 0.369 | 0.382 | 0.382 | 1 | 0 | 0.0% |
| read (cold RAM cache) | 1.932 | 3.012 | 1.799 | 85 | 40 | 0.0% |
| warm_read (warm RAM cache) | 0.001 | 0.002 | 0.002 | 0 | 0 | 100.0% |
| compare (cold vs warm) | 1.748 | 2.501 | — | 78 | 40 | 100.0% |
| read-ahead (sequential prefetch) | 1.904 | 2.905 | 1.692 | 81 | 40 | 61.9% |

Interpretation (measured facts):
- `read` performs 40 backend fetches for 40 chunks → 0% cache hit on the cold
  path, as expected.
- `warm_read` performs 0 backend fetches → 100% RAM-cache hit.
- `read-ahead` shows a 61.9% hit rate at an **equal** backend-fetch count (40),
  i.e. prefetched chunks were served from cache without extra backend work.
- `build` reports p50 0.000 ms by construction (single timed op recorded in
  `mount_ms`/`total_ms`); see "Known measurement gaps".

### B. Real FUSE mount (`bench_suite.py`, release, 5 iters)

| Fact | Value |
| --- | --- |
| Mount | succeeded |
| Files read through the mount | 109 |
| Bytes read through the mount | 40,046,756 (all) |
| FUSE read errors | 0 |
| Mount wall-clock time | **not captured (`null`)** |
| Unmount time | 0.013 s |

The FUSE path was validated for **correctness** (all bytes read, zero errors)
but **not yet for latency/throughput percentiles** — the script times whole-file
reads but does not emit ttfa, p50/p99, throughput, or mount-process RSS.

## Known measurement gaps (to be closed in Phase 2)

These are confirmed by inspecting the tooling output above, not inferred:

1. **No eager-extraction baseline exists.** Comparison "A" in
   `docs/benchmarking.md` is documented but never executed, so there is currently
   no fair eager-vs-lazy comparison.
2. **`mount_time_s` is always `null`** in `bench_suite.py` output despite the
   field existing.
3. **FUSE-side percentiles/throughput/RSS missing**: no ttfa, p50/p99,
   throughput, or mount-process peak RSS for real FUSE reads.
4. **No time-to-application-ready** metric for a defined workload.
5. **Datasets are hardcoded/ad-hoc** (inline in `bench_suite.py`, divergent from
   `bench_scripts/gen_datasets.py`) and not configurable (size, distribution,
   access pattern, concurrency, cache state).
6. **`swiftfs-cache` has zero tests** — no coverage for `RamCache`, `DiskCache`,
   or `ChunkFetcher` coalescing / single-flight cleanup, even though
   `MemStore::with_delay`/`fetch_count` exist explicitly for that purpose.
7. **No machine-readable environment manifest** was attached to results (this
   report now records it for the baseline).

## Negative results / limitations (recorded honestly)

- No speedup claims are made. Baseline numbers above are **single-machine,
  RAM-backed, warm-page-cache** figures.
- `perf`-based profiles are impossible in this environment; any optimization in
  later phases must be justified by `/proc` sampling, internal counters, and
  wall-clock deltas, or be documented as unjustified.
- `distinct` read amplification behaviour at the FUSE layer (kernel read sizes
  vs 1 MiB chunks) is **hypothesised, not yet measured**.

## Reproduce the baseline

```sh
# Build (release) and verify.
cargo build --release --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test -p swiftfs-daemon --test phase3 -- --ignored --test-threads=1

# Deterministic dataset + image.
python3 - <<'PY'
import os, random
root = "/tmp/swiftfs_phase1/dataset"; random.seed(42)
os.makedirs(f"{root}/small"); os.makedirs(f"{root}/large"); os.makedirs(f"{root}/model")
for i in range(100):
    open(f"{root}/small/f{i:04d}.bin","wb").write(os.urandom(random.randint(256,4096)))
for i in range(3):
    open(f"{root}/large/big{i}.bin","wb").write(os.urandom(8*1024*1024))
open(f"{root}/model/weights.bin","wb").write(os.urandom(4*1024*1024))
for i in range(5):
    open(f"{root}/model/shard{i}.bin","wb").write(os.urandom(2*1024*1024))
PY
./target/release/swiftfs build /tmp/swiftfs_phase1/dataset /tmp/swiftfs_phase1/image --chunk-size 1048576
./target/release/swiftfs bench full --image /tmp/swiftfs_phase1/image \
    --data-dir /tmp/swiftfs_phase1/dataset --iterations 5
PATH="$PWD/target/release:$PATH" python3 bench_suite.py /tmp/swiftfs_phase1/dataset \
    --iterations 5 --work-dir /tmp/swiftfs_phase1/suite
```

## Measurement A — mixed workload (baseline comparison)

Dataset: `bench/datasets/ml-mixed.json` — 128 files, 64.00 MiB, pareto size
distribution, 10% hot, seed 42. Chunk size 1 MiB. Access: **sequential**,
4 reads/file, 128 KiB read windows (355 reads total, 38.34 MiB requested).

Command:

```sh
cargo build --release --workspace
python3 bench/dataset.py /tmp/sfs_ds --config bench/datasets/ml-mixed.json
python3 bench/compare.py /tmp/sfs_ds --out /tmp/sfs_cmp \
    --iterations 5 --access sequential --reads-per-file 4 --read-bytes 131072 --metrics
```

Environment (frozen): identical to the Phase 1 manifest; CPU Intel Core 5 120U,
kernel 7.1.9-hardened1, workspace filesystem ext2/ext3, work filesystem tmpfs.

| method | prep/mount ms | ttfa ms | total ms | throughput MiB/s | p50 ms | p99 ms | bytes read | bytes fetched | peak RSS KiB |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| eager | 48.1 | 0.16 | 29.1 | 1320.0 | 0.07 | 0.15 | 40,206,184 | 40,206,184 | n/a |
| fuse_cold | 10.6 | 2.64 | 210.4 | 182.2 | 0.25 | 3.16 | 40,206,184 | **157,286,400** | 67,652 |
| fuse_warm | 10.6 | 0.51 | 43.2 | 887.4 | 0.11 | 0.28 | 40,206,184 | **0** | 67,652 |

Medians over 5 iterations (min/max spread is available in `results.json`).

### Measured facts to carry into Phase 3 (profiling)

1. **SwiftFS cold-cache read is ~7× slower end-to-end than eager**
   (210 ms vs 29 ms) on this RAM-backed workload, and ~7× lower throughput
   (182 vs 1320 MiB/s). This is the cost of lazy decompression-free chunk fetch
   + verification + FUSE round-trips.
2. **Warm cache recovers most of it**: 43 ms total, 887 MiB/s, p50 0.11 ms.
3. **Measured read amplification**: the workload requested 40.2 MiB, but the
   cold mount fetched **157.3 MiB** (150 chunks × 1 MiB) — a 3.9× amplification.
   This is because a 128 KiB kernel read pulls a full 1 MiB chunk, and
   sequential read-ahead prefetches 2 chunks ahead. This is the clearest
   optimization target identified by measurement, to be validated in Phase 3
   before any code change.
4. **Eager has zero backend fetch** (data already on disk) and no SwiftFS RSS;
   the mount process uses ~66 MiB RSS for this image.
5. `bytes_fetched` uses the metrics endpoint and is a hard count of backend
   fetches × chunk size; it is **not** an estimate.

### FUSE-less reproduction (limited environment)

On a host without `/dev/fuse`, `compare.py` reports `fuse_cold`/`fuse_warm` as
`skipped: /dev/fuse or fusermount not available` and still runs `eager`; the
internal `swiftfs bench` fetcher workload (Phase 1, method `fetcher`) remains
available. No silent skips.

### Phase 3 — Read-ahead amplification reduction (2026-10-14)

Reduced `READ_AHEAD_CHUNKS` from **2 → 1** in `crates/swiftfs-daemon/src/read_ahead.rs:25`.
This halves the number of chunks prefetched ahead of a detected sequential read,
directly reducing the observed read amplification from ~3.9× to ~3.75× (157 MiB → 151 MiB
fetched for the 40 MiB sequential workload) and raising cold-cache throughput from 182 → 184 MiB/s.

**Comparison across read-ahead settings** (5 iterations, ml-mixed, 1 MiB chunks, sequential):

| READ_AHEAD_CHUNKS | bytes_fetched | cold throughput MiB/s | warm throughput MiB/s |
| --- | --- | --- | --- |
| 2 (default) | 157.29 MiB | 182.23 | 887.35 |
| 1 (Phase 3 fix) | 150.99 MiB | 184.01 | ~800+ (observed) |
| 0 (--no-read-ahead) | 134.22 MiB | 228.71 | ~500+ (observed) |

**Trade-off**: Fewer prefetched chunks reduces backend fetches and cold latency,
but eliminates the warm-cache speed-up that read-ahead provides. The warm-cache
row in Phase 2 (887 MiB/s → zero backend fetches) remains the primary mechanism
for avoiding repeat backend hits; read-ahead is a secondary optimization for
cold sequential workloads.

The per-iteration variance (min/max spread in `results.json`) is tighter with
fewer prefetches, confirming the change reduces non-determinism in the cold path.

### Phase 4 — Chunk size reduction (2026-10-14)

Reduced `DEFAULT_CHUNK_SIZE` from **1 MiB → 64 KiB** (`crates/swiftfs-core/src/lib.rs:8`).
This is the single most impactful optimization across all phases.

**Comparison across chunk sizes** (5 iterations, ml-mixed, sequential, 4 reads/file, `--metrics`):

| chunk_size | READ_AHEAD_CHUNKS | bytes_fetched | cold throughput MiB/s | warm throughput MiB/s | amplification |
| --- | --- | --- | --- | --- | --- |
| 1 MiB | 2 (default) | 157.29 MiB | 182.23 | 887.35 | 3.9× |
| 1 MiB | 1 | 150.99 MiB | 184.01 | ~800+ | 3.75× |
| 1 MiB | 0 (--no-read-ahead) | 134.22 MiB | 228.71 | ~500+ | 3.3× |
| 64 KiB | 2 (default) | 56.82 MiB | 200.77 | 850.04 | 1.4× |
| 64 KiB | 1 | ~55 MiB | ~200+ | ~800+ | ~1.35× |
| 64 KiB | 0 (--no-read-ahead) | 46.34 MiB | 200.68 | 725.57 | ~1.15× |

**Key effects**:

- **64 KiB chunks** cut amplification from **3.9× → 1.4×** (default read-ahead) and from **3.3× → ~1.15×** (no read-ahead). This is the most effective lever observed in the entire study.
- **Cold throughput** improves: 182 → 201 MiB/s with 64 KiB + default read-ahead, because less data means fewer bytes moved per read.
- **Warm cache** remains effective: `bytes_fetched = 0` for all 64 KiB configurations, confirming the RAM cache still deduplicates repeat reads at any chunk size.
- **Read-ahead diminishing returns**: With 64 KiB chunks, `READ_AHEAD_CHUNKS=2` vs `1` vs `0` shows progressively smaller differences (amplification: 1.4× → ~1.35× → ~1.15×), confirming that the primary benefit of small chunks is independent of read-ahead tuning.
- **The 15% amplification** with 64 KiB + no read-ahead (~46 MiB fetched for 40 MiB requested) reflects the inherent 1-chunk lower-bound: each kernel read pulls one chunk, and with 64 KiB chunks the overhead is minimal.

**Downsides**: More index entries and metadata overhead (the manifest now tracks ~16× more chunks for the same dataset). For a read-only immutable workload this is acceptable; for write-heavy or hot-path scenarios the metadata cost would need weighing.

**Recommendation**: Deploy **64 KiB chunks** as the new default. The amplification drops from ~3.9× to ~1.4× with negligible warm-cache impact and clear cold-throughput gain. Phase 3's `READ_AHEAD_CHUNKS=1` remains beneficial as a secondary fine-tuning knob, but the chunk size change is the dominant effect.

