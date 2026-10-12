# Benchmarking

> Phase 6 deliverable. This document describes the intended methodology; it does
> **not** report results yet. No performance numbers will be claimed until they
> are produced by the harness on a recorded machine configuration.

## Principle

Do not fabricate numbers. Every reported figure must be traceable to a
reproducible command, a dataset generator, a cache state, and a machine
description.

## Comparisons

- **A.** Eager extraction to a normal filesystem.
- **B.** SwiftFS with cold caches.
- **C.** SwiftFS with warm caches.
- **D.** SwiftFS read-ahead disabled vs enabled.
- **E.** SwiftFS across chunk sizes and concurrency limits.

## Workloads

- Many small files.
- A few very large files.
- Sparse access to a large image.
- Sequential reads.
- Random reads.
- Repeated hot-file access.
- Concurrent reads from multiple processes.
- Model-artifact-style dataset and multi-shard model load.

## Metrics

- Mount / preparation time.
- Time to first successful file access.
- Time to first application-ready state.
- Bytes fetched before the first useful operation.
- Total completion time and throughput.
- p50 and p99 read latency.
- Peak RSS.
- CPU utilization (when measurable).
- Cache hit rate.
- Number of storage requests.

## Guardrails

- Repeatable workloads, warm-up rules, controlled cache state, multiple runs.
- Record machine/environment metadata with each result.
- Store raw results as CSV/JSON; generate summary tables/plots from raw data.
- Distinguish mount time from application startup, and file-access latency from
  model-load time. Synthetic benchmarks are not proof of GPU inference speed.

## Tooling availability

This environment lacked `strace`, `perf`, `iostat`, and `/usr/bin/time`. The
report will state which profiling tools were available and used, and provide
reproducible commands for those that were not.
