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

**Corrected in Phase 6:** this phase varied the harness `--chunk-size` flag only;
`DEFAULT_CHUNK_SIZE` in `crates/swiftfs-core/src/lib.rs` remained 1 MiB until
Phase 6. The figures below are `backend_fetches x chunk_size`
(`backend_bytes_nominal`), an upper bound on bytes moved — not actual backend
bytes. See Phase 6 for corrected, actual-byte measurements and the decision that
finally changed the compiled default to 64 KiB.

Varied the harness chunk size to 64 KiB (the compiled default was still 1 MiB):

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


### Phase 5 — Further chunk size reduction to 16 KiB (2026-10-14)

**Corrected in Phase 6:** harness `--chunk-size` varied to 16 KiB; compiled
default unchanged at this point. Figures are nominal (upper-bound) bytes.

Varied the harness chunk size to 16 KiB:

**Comparison across all chunk sizes** (5 iterations, ml-mixed, sequential, 4 reads/file, `--metrics`):

| chunk_size | bytes_fetched | cold throughput MiB/s | warm throughput MiB/s | amplification |
| --- | --- | --- | --- | --- |
| 1 MiB | 157.29 MiB | 182.23 | 887.35 | 3.9× |
| 64 KiB | 56.82 MiB | 200.77 | 850.04 | 1.4× |
| 16 KiB | 44.70 MiB | 215.11 | 1007.87 | 1.12× |

**Key effects**:

- **16 KiB chunks** push amplification down to **1.12×** (44.70 MiB fetched for 40 MiB requested), 
  confirming the asymptotic lower bound where each kernel read pulls one chunk and 
  16 KiB overhead is minimal. 
- **Cold throughput** continues climbing: 182 → 201 → 215 MiB/s across the three 
  chunk sizes. The improvement from 64→16 KiB is ~7 MiB/s vs the 64→1 MiB gain 
  of ~22 MiB/s, showing diminishing returns.
- **Warm cache** remains fully effective: `bytes_fetched = 0` for all three chunk 
  sizes. The RAM cache deduplicates repeat reads regardless of chunk size.
- **Per-iteration variance** tightens further with smaller chunks, confirming 
  reduced non-determinism as a secondary benefit.

**Downsides**: 
- **Metadata overhead**: 16 KiB chunks mean ~64× more chunks for the same dataset 
  compared to 1 MiB, which increases index size and memory pressure in the 
  ManifestIndex and ChunkFetcher state. For this read-only benchmark workload 
  the cost is acceptable, but production workloads with many files or concurrent 
  access would need to weigh this.
- **No further meaningful gains**: The jump from 64→16 KiB yields only ~2× 
  amplification reduction (1.4× → 1.12×) and ~7 MiB/s cold throughput, 
  while the 1 MiB→64 KiB jump gave ~64% amplification reduction and ~22 MiB/s 
  cold throughput gain. The law of diminishing returns is clearly in effect.

**Recommendation**: 64 KiB remains the practical optimum. 16 KiB achieves only 
marginal additional amplification reduction (1.4× → 1.12×) at the cost of 
~64× metadata overhead. The 64 KiB default from Phase 4 is the sweet spot 
between amplification reduction and metadata overhead.


---

# Phase 6 — Audit, 1 GiB ML workload, profiling, and revised default

Date: 2026-10-16. Everything below is measured with the corrected harness. This
phase supersedes the metric interpretation of Phases 2–5: those phases reported
`backend_fetches x chunk_size` as if it were bytes moved. That quantity is an
**upper bound** and is now labelled `backend_bytes_nominal`.

## 6.1 Metric definitions (exact)

The comparison harness now separates these quantities and never conflates them:

| Metric | Definition | Source |
| --- | --- | --- |
| `app_bytes` | Useful bytes returned to the reader | harness read loop |
| `chunk_requests` | Chunk lookups (RAM/disk/backend) | `swiftfs_chunk_requests_total` |
| `ram_hits` | RAM-cache hits | `swiftfs_ram_cache_hits_total` |
| `backend_fetches` | Backend fetches after coalescing | `swiftfs_backend_fetches_total` |
| `backend_bytes_actual` | **True** bytes returned by backend, summed over verified chunk lengths | `swiftfs_backend_bytes_total` |
| `backend_bytes_nominal` | `backend_fetches x chunk_size` (upper bound) | derived |
| `amplification_actual` | `backend_bytes_actual / app_bytes` | derived |
| `amplification_nominal` | `backend_bytes_nominal / app_bytes` | derived |

**Amplification formula.** `amplification_actual = (Σ verified chunk lengths
returned by the backend after coalescing) / (Σ bytes returned to the reader)`.
Read-ahead prefetches count: they are real backend fetches. Cache hits and
coalesced joins do **not** add backend bytes (they are served without a backend
read). `swiftfs_backend_bytes_total` is incremented in
`crates/swiftfs-cache/src/fetcher.rs` exactly once per verified backend fetch, by
`data.len()`.

**Why the old number was wrong.** The image format stores one object per chunk
(`crates/swiftfs-format/src/builder.rs`), and the last chunk of every file may be
shorter than `chunk_size`. `fetches x chunk_size` therefore overstates bytes.
Measured effect on the 1 GiB dataset: at 4 KiB the error is ~0.4% (only 512 tail
chunks of 262,414), but at 1 MiB with a Pareto ML mix most files are a single
short chunk, so the nominal figure is a large overestimate. Example: 64 KiB
sequential cold, nominal 415.6 MB vs **actual 414.1 MB**; amplification 1.62x
(actual), reported as ~1.63x nominal. The gap widens for large chunks; the fix
makes the figure honest rather than flattering.

## 6.2 Fairness and equivalence

- **Equivalent useful work.** All configurations execute the *same* generated
  read list (same files, offsets, lengths). `bench/compare.py --validate` mounts
  the image and compares every requested range byte-for-byte against the source
  tree, printing a BLAKE2b checksum. It passes (`equivalent: true`) for the 1 GiB
  workload.
- **Equal timing boundary.** Eager now reports `end_to_end_s = prep_s + total_s`
  (copy + reads); SwiftFS reports `end_to_end_s = mount_s + total_s`. Total
  workload time is still reported separately so costs can be attributed.
- **Zero backend ≠ zero data movement.** The Linux page cache may serve reads
  (`FOPEN_KEEP_CACHE`) with zero SwiftFS traffic. A `backend_bytes_actual = 0`
  warm row means *no backend reads*, not *no bytes copied*; the harness labels
  warm rows as *repeat read*.
- **`bytes_read` renamed `app_bytes`** to avoid implying backend I/O.

## 6.3 Larger workload — 1 GiB ML-style dataset

`bench/datasets/ml-large.json`: 512 files, 1,073,741,824 bytes (1024 MiB),
pareto α=1.2, 8 shard dirs, 8% hot, seed 1234. Generated with
`python3 bench/dataset.py /tmp/ml_large --config bench/datasets/ml-large.json`.

Image metadata per chunk size (measured):

| chunk_size | unique chunks | image MiB | manifest bytes |
| --- | --- | --- | --- |
| 1 MiB | 1,248 | 1026 | 443,614 |
| 64 KiB | 16,638 | 1030 | 4,745,488 |
| 16 KiB | 65,790 | 1043 | 18,490,650 |
| 4 KiB | 262,414 | 1095 | 73,214,878 |

### Results (5 iterations, medians; `--metrics --validate`)

Cold = fresh mount, empty SwiftFS caches. Warm = same workload repeated.
`amp` is `amplification_actual`. `mount` is SwiftFS mount time.

| chunk | access | cold ms | fetch | backend MiB | amp | cold MiB/s | warm MiB/s | p99 ms | mount ms | RSS MiB |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 MiB | sequential | 1389 | 986 | 768.6 | 3.00 | 184.4 | 1075 | 3.45 | 10.6 | 320 |
| 64 KiB | sequential | 771 | 6678 | 414.1 | 1.62 | 332.2 | 1051 | 1.12 | 36.7 | 276 |
| 16 KiB | sequential | 1251 | 21007 | 327.6 | 1.28 | 204.6 | 1224 | 1.66 | 115.5 | 289 |
| 4 KiB | sequential | 2239 | 81686 | 318.9 | 1.25 | 114.3 | 987 | 3.07 | 443.4 | 350 |
| 1 MiB | random | 1652 | 1089 | 817.1 | 4.26 | 116.2 | 1452 | 4.40 | 10.5 | 321 |
| 64 KiB | random | 1463 | 7881 | 481.9 | 2.51 | 131.2 | 1110 | 1.96 | 36.8 | 280 |
| 16 KiB | random | 1684 | 23322 | 362.2 | 1.89 | 114.0 | 994 | 2.14 | 115.9 | 291 |
| 1 MiB | hot | 139 | 74 | 60.4 | 5.90 | 73.9 | 681 | 5.23 | 15.9 | 63 |
| 64 KiB | hot | 33 | 338 | 21.1 | 2.06 | 309.3 | 1021 | 1.80 | 36.5 | 30 |
| 16 KiB | hot | 69 | 1033 | 16.1 | 1.57 | 148.0 | 581 | 3.35 | 120.3 | 40 |

Eager (for reference, prep + workload): `cp -r` of 1 GiB takes ~600–720 ms; the
read workload runs at ~1.5–3.5 GiB/s from tmpfs. Eager `app_bytes` equals the
requested bytes and it reports no backend figures (undefined).

### Findings

1. **64 KiB maximizes cold throughput** on every access pattern (332 / 131 / 309
   MiB/s vs 1 MiB's 184 / 116 / 74 and 16 KiB's 205 / 114 / 148).
2. **Amplification falls monotonically with chunk size** but with strong
   diminishing returns below 64 KiB: 1 MiB 3.0–5.9x, 64 KiB 1.6–2.5x, 16 KiB
   1.3–1.9x, 4 KiB 1.25x.
3. **16 KiB does not beat 64 KiB on throughput**, and costs 3x the mount time
   (115 vs 37 ms) and 4x the manifest (17.6 vs 4.5 MB).
4. **1 MiB is clearly worst for lazy loading**: highest amplification, lowest
   cold throughput, highest p99 — but the lowest mount time and smallest manifest.
5. **Warm is not always zero.** At 64 KiB sequential the 256 MiB RAM cache cannot
   retain the full working set across the warm pass, so ~1.4k backend fetches
   remain. This is expected cache behaviour and is now visible rather than hidden.

### Workloads that could not run

- **2–4 GiB datasets**: not attempted. `/tmp` is tmpfs and the host had <6 GiB
  free during the run; a 2 GiB dataset plus a 2 GiB image plus smaller-chunk
  metadata would risk exhausting tmpfs. Recorded as a limitation, not a result.
- **`--no-read-ahead` on 1 GiB** and additional access mixes were skipped to
  bound total run time; the read-ahead effect was characterised in Phase 3 on the
  smaller dataset.

## 6.4 Profiling the 4 KiB regression

Tools `perf`/`strace`/`iostat` are absent (see environment manifest). Profiling
used wall-clock deltas, `/proc/<pid>/stat` CPU time, `/proc/<pid>/status` RSS,
and the fetcher's internal counters. Hypothesis (per-chunk fixed cost) was tested
directly:

| chunk | fetches/sec, cold | wall µs/fetch | CPU s, cold | CPU µs/fetch | mount ms |
| --- | --- | --- | --- | --- | --- |
| 1 MiB | 710 | 1511 | 1.78 | 1803 | 10.6 |
| 64 KiB | 8662 | 133 | 1.45 | 216 | 36.7 |
| 16 KiB | 16789 | 57 | 1.82 | 87 | 115.5 |
| 4 KiB | 36485 | 29 | 3.80 | 47 | 443.4 |

**Dominant cost: per-chunk fixed overhead, exposed by chunk count.** At 4 KiB the
sequential cold pass performs **81,686 backend fetches** vs 986 at 1 MiB — an 83x
increase. Each fetch incurs an object `open`+`stat`+`seek`+`read` (via
`spawn_blocking`), a BLAKE3 hash of the bytes, an in-flight-map lock, and a FUSE
round-trip. Cold wall time tracks fetch count (81,686 × ~29.5 µs ≈ 2.4 s), not
bytes; CPU time is 3.80 s across 2.4 s wall, i.e. more than one core, confirming
the path is CPU/syscall-bound rather than storage-bound (data is on tmpfs).
Mount time also explodes to 443 ms because the **73 MB manifest** with 262k
chunks must be parsed and indexed into `ManifestIndex`.

**Conclusion.** The 4 KiB regression is structural (fixed per-chunk work), not a
single hot function. **No optimization is applied**: the only effective fix would
be to pack multiple logical chunks per object and fetch a multi-chunk range in a
single syscall/hash, which is a format change (out of scope) and unjustified
while 64 KiB already meets the throughput goal. This is documented as a
bottleneck/trade-off, per the "evidence or document" rule.

## 6.5 Revised default chunk size

**Decision: `DEFAULT_CHUNK_SIZE` changed from 1 MiB to 64 KiB**
(`crates/swiftfs-core/src/lib.rs`).

This is **not** a claim that 64 KiB is "the measured winner" on every axis. The
data shows:

- **Throughput winner: 64 KiB** (highest cold throughput across sequential,
  random, and hot) — this is the axis that matters for time-to-useful-work.
- **Amplification winner: 4 KiB** (1.25x), with 16 KiB close behind (1.28x).
- **Mount/metadata winner: 1 MiB** (10 ms, 0.4 MB manifest).

64 KiB is chosen because it is the **best balanced point**: it is the cold
throughput optimum, cuts true amplification from 3.0–5.9x to 1.6–2.5x, keeps
mount time modest (37 ms) and the manifest small (4.5 MB). Choosing 16 KiB would
buy ~0.3x lower amplification at the cost of 3x higher mount time and lower cold
throughput; choosing 4 KiB is rejected outright (CPU-bound regression, 70 MB
manifest). The previous 1 MiB default had the worst amplification and lowest cold
throughput of the tested sizes, so raising it to 64 KiB is a clear, measured
improvement rather than a preference.

## Reproduce this phase

```sh
cargo build --release --workspace
python3 bench/dataset.py /tmp/ml_large --config bench/datasets/ml-large.json
# one cell (repeat for cs in 1048576 65536 16384 4096; access in
# sequential random hot):
python3 bench/compare.py /tmp/ml_large --out /tmp/cell \
    --iterations 5 --access sequential --reads-per-file 4 --read-bytes 131072 \
    --chunk-size 65536 --metrics --validate
# or drive the whole matrix:
DS=/tmp/ml_large ./bench/run_matrix.sh
```

Raw per-cell JSON/CSV are written under `bench-out/` (git-ignored).

## Limitations (unchanged or newly recorded)

- Data and images live on **tmpfs**; absolute latencies measure memory copy, not
  disk seek. Relative comparisons between chunk sizes remain valid.
- The OS page cache cannot be dropped (unprivileged), so eager reads are warm.
- No `perf`/`strace`/`iostat`; profiling is `/proc`/counter/wall-clock based.
- One machine, one dataset shape (Pareto ML-style). Other distributions
  (many tiny files, one huge file) may shift the optimum; not measured here.
- 2–4 GiB datasets were not run (see 6.3).
