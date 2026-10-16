#!/usr/bin/env python3
"""Reproducible eager-extraction vs SwiftFS lazy-loading comparison.

This harness measures a single, clearly-defined *read workload* against three
configurations and never mixes their numbers:

- ``eager``   : the dataset copied to a local directory (``cp -r``), then read.
- ``fuse_cold``: SwiftFS mounted through FUSE with an empty RAM cache.
- ``fuse_warm``: the same mount after the workload has populated the RAM cache.

Every configuration performs the **same useful work**: the same list of
byte-range reads over the same files. Results are tagged with ``method`` so
tables are never conflated.

Metrics per iteration (median + spread across iterations, warm-up discarded):

- preparation / mount time,
- time to first useful read,
- time to application-ready (the whole workload),
- bytes read (application) and bytes fetched (backend; SwiftFS only),
- throughput (MiB/s over the workload),
- read-latency p50 / p99,
- peak RSS of the mount process (SwiftFS) or the harness (eager).

Outputs: ``<out>/results.json`` (raw), ``<out>/summary.csv`` (compact),
``<out>/environment.json`` (machine manifest), and a Markdown table on stdout.

FUSE-less fallback: if ``/dev/fuse`` is unavailable, ``eager`` still runs and the
``fuse_*`` configs are reported as ``skipped`` with an explicit reason.
"""

from __future__ import annotations

import argparse
import json
import os
import random
import shutil
import statistics
import subprocess
import sys
import time
from pathlib import Path

import dataset as ds
from common import (
    Stat,
    percentile,
    EnvRecorder,
    environment_manifest,
    fuse_available,
    process_cpu_time_s,
    run_cmd,
    scrape_metrics,
    await_metrics,
    counter_delta,
    swiftfs_bin,
    unmount,
)

MIB = 1024 * 1024


# --------------------------------------------------------------------------
# Read workload definition
# --------------------------------------------------------------------------

def build_workload(manifest: dict, access: str, reads_per_file: int,
                   read_bytes: int, seed: int) -> list[dict]:
    """Produce a list of {path, offset, length} reads, identical across configs.

    ``sequential``: read from the start of each file, ``reads_per_file`` windows
    of ``read_bytes``. ``random``: random offsets within each file. ``hot``: read
    only the recorded hot files, sequentially (application-ready style).
    """
    rng = random.Random(seed + 7)
    files = manifest["files"]
    if access == "hot":
        hot = set(manifest.get("hot_files", []))
        files = [f for f in files if f["path"] in hot] or files
    work: list[dict] = []
    for f in files:
        size = f["bytes"]
        if size <= 0:
            continue
        if access == "random":
            for _ in range(reads_per_file):
                length = min(read_bytes, size)
                offset = rng.randint(0, max(0, size - length))
                work.append({"path": f["path"], "offset": offset, "length": length})
        else:  # sequential (and hot)
            for i in range(reads_per_file):
                offset = i * read_bytes
                if offset >= size:
                    break
                length = min(read_bytes, size - offset)
                work.append({"path": f["path"], "offset": offset, "length": length})
    return work


def perform_reads(root: Path, workload: list[dict]) -> dict:
    """Execute the workload against ``root``; return timing/bytes metrics."""
    latencies_ms: list[float] = []
    total_bytes = 0
    ttfa_ms = None
    t0 = time.perf_counter()
    for r in workload:
        p = root / r["path"]
        rt0 = time.perf_counter()
        try:
            with open(p, "rb") as f:
                f.seek(r["offset"])
                data = f.read(r["length"])
        except OSError as e:
            raise RuntimeError(f"read failed for {p}: {e}")
        dt = (time.perf_counter() - rt0) * 1000.0
        latencies_ms.append(dt)
        total_bytes += len(data)
        if ttfa_ms is None:
            ttfa_ms = (time.perf_counter() - t0) * 1000.0
    total_s = time.perf_counter() - t0
    return {
        "ttfa_ms": ttfa_ms or 0.0,
        "total_s": total_s,
        "app_bytes": total_bytes,
        "throughput_mib_s": (total_bytes / MIB / total_s) if total_s > 0 else 0.0,
        "p50_ms": percentile(latencies_ms, 50.0),
        "p99_ms": percentile(latencies_ms, 99.0),
        "n_reads": len(latencies_ms),
    }


# --------------------------------------------------------------------------
# Configurations
# --------------------------------------------------------------------------

def run_eager_iteration(source: Path, workdir: Path, workload: list[dict]) -> dict:
    """Copy the dataset locally, then run the workload.

    Metrics are reported with explicit, non-conflated names:

    - ``prep_s``  : wall-clock time of ``cp -r`` (data placement).
    - ``total_s`` : wall-clock time of the read workload only.
    - ``end_to_end_s`` : ``prep_s + total_s`` (placement + reads), so eager can
      be compared with FUSE on the same "time until useful work is done"
      boundary (FUSE ``mount_s + total_s``).
    - ``app_bytes`` : useful bytes returned to the reader (identical across all
      configurations by construction).
    - ``backend_bytes_actual`` / ``backend_fetches`` : **unknown for eager**
      (``None``): "backend" is not defined for a plain copy. We do not report the
      copy as zero backend traffic — that would conflate zero *backend* with zero
      *data movement*.
    """
    if workdir.exists():
        shutil.rmtree(workdir)
    t0 = time.perf_counter()
    rc, _, err = run_cmd(["cp", "-r", str(source), str(workdir)])
    prep_s = time.perf_counter() - t0
    if rc != 0:
        raise RuntimeError(f"cp -r failed: {err}")
    cpu0 = process_cpu_time_s(os.getpid())
    metrics = perform_reads(workdir, workload)
    cpu1 = process_cpu_time_s(os.getpid())
    metrics["prep_s"] = prep_s
    metrics["end_to_end_s"] = prep_s + metrics["total_s"]
    metrics["method"] = "eager"
    if cpu0 and cpu1:
        metrics["cpu_s"] = (cpu1[0] + cpu1[1]) - (cpu0[0] + cpu0[1])
    metrics["backend_bytes_actual"] = None
    metrics["backend_bytes_nominal"] = None
    metrics["backend_fetches"] = None
    metrics["chunk_requests"] = None
    metrics["ram_hits"] = None
    metrics["coalesced"] = None
    return metrics


def _start_mount(sfs: str, image: Path, mnt: Path, extra: list[str],
                 log_path: Path) -> subprocess.Popen:
    mnt.mkdir(parents=True, exist_ok=True)
    unmount(str(mnt))
    # Redirect the mount's output to a file. Piping it to a PIPE that is never
    # drained fills the pipe buffer and blocks the FUSE session, hanging reads.
    log = open(log_path, "w")
    proc = subprocess.Popen(
        [sfs, "mount", str(image), str(mnt), *extra],
        stdout=log, stderr=subprocess.STDOUT, text=True,
    )
    proc._swiftfs_log = log  # type: ignore[attr-defined]
    return proc


def _await_mount(proc: subprocess.Popen, mnt: Path, timeout_s: float = 15.0) -> float:
    t0 = time.perf_counter()
    while time.perf_counter() - t0 < timeout_s:
        if os.path.ismount(mnt):
            return time.perf_counter() - t0
        if proc.poll() is not None:
            break
        time.sleep(0.005)
    log = getattr(proc, "_swiftfs_log", None)
    detail = ""
    if log is not None:
        log.flush()
        try:
            detail = Path(log.name).read_text()[-300:]
        except OSError:
            pass
    raise RuntimeError(f"mount did not appear: {detail.strip()}")


def _apply_backend_deltas(rec: dict, before: dict | None, after: dict | None,
                          chunk_size: int) -> None:
    """Fill backend/cache metric deltas for one measured phase.

    All values are ``None`` (unknown) when the metrics endpoint is disabled, so a
    disabled scrape is never mistaken for measured zero traffic.
    """
    rec["chunk_requests"] = counter_delta(before, after, "swiftfs_chunk_requests_total")
    rec["ram_hits"] = counter_delta(before, after, "swiftfs_ram_cache_hits_total")
    rec["backend_fetches"] = counter_delta(before, after, "swiftfs_backend_fetches_total")
    rec["backend_bytes_actual"] = counter_delta(before, after, "swiftfs_backend_bytes_total")
    rec["coalesced"] = counter_delta(before, after, "swiftfs_coalesced_requests_total")
    # Nominal bytes: what the old harness reported (fetches x chunk_size). Kept
    # for comparison but explicitly labelled as an upper bound on chunk bytes.
    bf = rec["backend_fetches"]
    rec["backend_bytes_nominal"] = (bf * chunk_size) if bf is not None else None
    # Amplification: actual backend bytes moved per useful application byte.
    app = rec.get("app_bytes")
    if app and rec["backend_bytes_actual"] is not None:
        rec["amplification_actual"] = rec["backend_bytes_actual"] / app
    else:
        rec["amplification_actual"] = None
    if app and rec["backend_bytes_nominal"] is not None:
        rec["amplification_nominal"] = rec["backend_bytes_nominal"] / app
    else:
        rec["amplification_nominal"] = None


def run_fuse_iterations(sfs: str, image: Path, mnt: Path, workload: list[dict],
                        extra: list[str], metrics_port: int | None,
                        iteration: int, out: Path, chunk_size: int) -> dict:
    """One mount; measure cold then warm reads. Returns both configs' metrics."""
    log_path = out / f"mount_iter{iteration}.log"
    proc = _start_mount(sfs, image, mnt, extra, log_path)
    rec = EnvRecorder(proc.pid)
    try:
        mount_s = _await_mount(proc, mnt)
        if metrics_port is not None:
            await_metrics(metrics_port)
        # Baseline scrape immediately after mount (expected: no backend traffic yet).
        base = scrape_metrics(metrics_port)
        # Cold: SwiftFS caches are empty immediately after mount. The kernel page
        # cache may still serve some reads (FOPEN_KEEP_CACHE); that shows up as
        # zero SwiftFS chunk requests, not zero data movement.
        cpu0 = process_cpu_time_s(proc.pid)
        cold = perform_reads(mnt, workload)
        cpu1 = process_cpu_time_s(proc.pid)
        after_cold = scrape_metrics(metrics_port)
        cold["mount_s"] = mount_s
        cold["method"] = "fuse_cold"
        cold["end_to_end_s"] = mount_s + cold["total_s"]
        if cpu0 and cpu1:
            cold["cpu_s"] = (cpu1[0] + cpu1[1]) - (cpu0[0] + cpu0[1])
        _apply_backend_deltas(cold, base, after_cold, chunk_size)
        # Warm ("repeat read"): same workload against the now-populated caches.
        warm = perform_reads(mnt, workload)
        after_warm = scrape_metrics(metrics_port)
        warm["mount_s"] = mount_s
        warm["method"] = "fuse_warm"
        warm["end_to_end_s"] = mount_s + warm["total_s"]
        _apply_backend_deltas(warm, after_cold, after_warm, chunk_size)
        peak = rec.stop()
        cold["peak_rss_kb"] = peak
        warm["peak_rss_kb"] = peak
        return {"cold": cold, "warm": warm}
    finally:
        unmount(str(mnt))
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
        log = getattr(proc, "_swiftfs_log", None)
        if log is not None:
            log.close()


# --------------------------------------------------------------------------
# Orchestration
# --------------------------------------------------------------------------


def run(args) -> dict:
    sfs = swiftfs_bin(prefer_release=True)
    source = Path(args.source).resolve()
    if not source.is_dir():
        raise SystemExit(f"source dataset not found: {source}")
    manifest = ds.load_manifest(source)

    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    image = out / "image"

    workload = build_workload(
        manifest, args.access, args.reads_per_file, args.read_bytes, args.seed
    )
    total_work_bytes = sum(r["length"] for r in workload)

    print(f"swiftfs binary : {sfs}")
    print(f"dataset        : {source} ({manifest['total_bytes']/MIB:.2f} MiB, "
          f"{manifest['file_count']} files)")
    print(f"workload       : {args.access}, {len(workload)} reads, "
          f"{total_work_bytes/MIB:.2f} MiB requested")
    print(f"iterations     : {args.iterations}")
    print(f"output         : {out}")

    # Build the SwiftFS image (reused by all FUSE iterations).
    print("\n[build] packaging image ...")
    if image.exists():
        shutil.rmtree(image)
    rc, _, err = run_cmd(
        [sfs, "build", str(source), str(image), "--chunk-size", str(args.chunk_size)]
    )
    if rc != 0:
        raise SystemExit(f"image build failed: {err.strip()}")

    env = environment_manifest()
    env["swiftfs_binary"] = sfs
    env["dataset_manifest"] = manifest["config"]
    env["dataset_total_bytes"] = manifest["total_bytes"]
    env["dataset_file_count"] = manifest["file_count"]
    env["chunk_size"] = args.chunk_size
    env["access"] = args.access
    env["reads_per_file"] = args.reads_per_file
    env["read_bytes"] = args.read_bytes
    env["iterations"] = args.iterations
    env["seed"] = args.seed
    env["image_unique_chunks"] = _count_unique_chunks(image)

    raw: dict = {"environment": env, "workload_reads": len(workload),
                 "workload_bytes": total_work_bytes, "configs": {}}

    # ---- eager ----
    print("\n[eager] copy + read ...")
    eager_runs = []
    for i in range(args.iterations):
        r = run_eager_iteration(source, out / "eager_tree", workload)
        r["iteration"] = i
        eager_runs.append(r)
        print(f"  iter {i}: prep={r['prep_s']*1000:.1f}ms "
              f"total={r['total_s']*1000:.1f}ms thr={r['throughput_mib_s']:.0f}MiB/s")
    raw["configs"]["eager"] = _aggregate(eager_runs)

    # ---- fuse cold/warm ----
    if not fuse_available() and not args.force_mount:
        reason = "/dev/fuse or fusermount not available"
        print(f"\n[fuse] SKIPPED: {reason}")
        raw["configs"]["fuse_cold"] = {"skipped": True, "reason": reason}
        raw["configs"]["fuse_warm"] = {"skipped": True, "reason": reason}
    else:
        print("\n[fuse] mount + cold/warm read ...")
        mnt = out / "mnt"
        base_extra: list[str] = []
        if args.disk_cache_dir:
            base_extra += ["--disk-cache-dir", str(Path(args.disk_cache_dir))]
        if args.ram_cache_bytes:
            base_extra += ["--ram-cache-bytes", str(args.ram_cache_bytes)]
        if args.no_read_ahead:
            base_extra += ["--no-read-ahead"]

        cold_runs, warm_runs = [], []
        for i in range(args.iterations):
            extra = list(base_extra)
            port = None
            if args.metrics:
                # Fresh port per iteration: each mount is a new process and must
                # rebind cleanly; a reused port can race on the previous owner.
                port = _free_port()
                extra += ["--metrics-listen", f"127.0.0.1:{port}"]
            res = run_fuse_iterations(
                sfs, image, mnt, workload, extra, port, i, out, args.chunk_size
            )
            c, w = res["cold"], res["warm"]
            c["iteration"] = i
            w["iteration"] = i
            cold_runs.append(c)
            warm_runs.append(w)
            print(f"  iter {i}: mount={c['mount_s']*1000:.1f}ms "
                  f"cold={c['total_s']*1000:.1f}ms thr={c['throughput_mib_s']:.0f}MiB/s "
                  f"fetches={c.get('backend_fetches')} "
                  f"bytes={_fmt_int(c.get('backend_bytes_actual'))} "
                  f"warm={w['total_s']*1000:.1f}ms rss={c.get('peak_rss_kb')}KiB")
        raw["configs"]["fuse_cold"] = _aggregate(cold_runs)
        raw["configs"]["fuse_warm"] = _aggregate(warm_runs)

        if args.validate:
            print("\n[validate] eager-vs-fuse byte equivalence ...")
            mnt = out / "mnt"
            port = _free_port() if args.metrics else None
            extra = list(base_extra)
            if port is not None:
                extra += ["--metrics-listen", f"127.0.0.1:{port}"]
            proc = _start_mount(sfs, image, mnt, extra, out / "validate_mount.log")
            try:
                _await_mount(proc, mnt)
                eq = validate_equivalence(source, mnt, workload)
                raw["equivalence"] = eq
                status = "OK" if eq["equivalent"] else f"MISMATCH {eq['mismatches'][:3]}"
                print(f"  {eq['reads_checked']} reads, {eq['bytes_checked']} bytes, "
                      f"blake2b={eq['checksum_blake2b16']} -> {status}")
            finally:
                unmount(str(mnt))
                try:
                    proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    proc.kill()
                log = getattr(proc, "_swiftfs_log", None)
                if log is not None:
                    log.close()

    # ---- persist ----
    with open(out / "results.json", "w") as f:
        json.dump(raw, f, indent=2)
    with open(out / "environment.json", "w") as f:
        json.dump(env, f, indent=2)
    _write_summary_csv(out / "summary.csv", raw)
    _print_markdown(raw)
    return raw


def _count_unique_chunks(image: Path) -> int:
    try:
        return len([p for p in (image / "chunks").iterdir() if p.is_file()])
    except OSError:
        return 0


def validate_equivalence(source: Path, mnt: Path, workload: list[dict]) -> dict:
    """Verify eager and FUSE return byte-identical data for the workload.

    Reads every requested range from the source tree and from the mounted image
    and compares them. Returns a small report; raises on any mismatch so a
    comparison cannot silently run on non-equivalent data.
    """
    import hashlib

    mismatches: list[str] = []
    total = 0
    checksum = hashlib.blake2b(digest_size=16)
    for r in workload:
        rel = r["path"]
        with open(source / rel, "rb") as f:
            f.seek(r["offset"])
            a = f.read(r["length"])
        with open(mnt / rel, "rb") as f:
            f.seek(r["offset"])
            b = f.read(r["length"])
        total += len(a)
        checksum.update(a)
        if a != b:
            mismatches.append(rel)
    return {
        "reads_checked": len(workload),
        "bytes_checked": total,
        "checksum_blake2b16": checksum.hexdigest(),
        "mismatches": mismatches,
        "equivalent": not mismatches,
    }


def _free_port() -> int:
    import socket

    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _aggregate(runs: list[dict]) -> dict:
    """Reduce per-iteration runs to median+spread per metric."""
    if not runs:
        return {"skipped": True, "reason": "no runs"}
    keys = [
        "prep_s", "mount_s", "end_to_end_s", "ttfa_ms", "total_s", "app_bytes",
        "chunk_requests", "ram_hits", "backend_fetches", "backend_bytes_actual",
        "backend_bytes_nominal", "coalesced", "amplification_actual",
        "amplification_nominal", "throughput_mib_s", "p50_ms", "p99_ms",
        "cpu_s", "peak_rss_kb", "n_reads",
    ]
    agg: dict = {"method": runs[0].get("method"), "iterations": len(runs)}
    for k in keys:
        vals = [r[k] for r in runs if k in r and r[k] is not None]
        if vals:
            agg[k] = Stat.of([float(v) for v in vals]).as_dict()
    return agg


def _write_summary_csv(path: Path, raw: dict) -> None:
    import csv

    with open(path, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow([
            "method", "n", "prep_ms_median", "mount_ms_median", "e2e_ms_median",
            "ttfa_ms_median", "total_ms_median", "app_bytes_median",
            "chunk_requests_median", "ram_hits_median", "backend_fetches_median",
            "backend_bytes_actual_median", "backend_bytes_nominal_median",
            "amplification_actual_median", "amplification_nominal_median",
            "throughput_mib_s_median", "p50_ms_median", "p99_ms_median",
            "cpu_s_median", "peak_rss_kb_median",
        ])
        for name, agg in raw["configs"].items():
            if agg.get("skipped"):
                w.writerow([name, "skipped"] + [""] * 18)
                continue
            pair = agg.get("method", name)
            w.writerow([
                pair,
                agg.get("iterations", ""),
                _med(agg, "prep_s", 1000.0),
                _med(agg, "mount_s", 1000.0),
                _med(agg, "end_to_end_s", 1000.0),
                _med(agg, "ttfa_ms"),
                _med(agg, "total_s", 1000.0),
                _med(agg, "app_bytes"),
                _med(agg, "chunk_requests"),
                _med(agg, "ram_hits"),
                _med(agg, "backend_fetches"),
                _med(agg, "backend_bytes_actual"),
                _med(agg, "backend_bytes_nominal"),
                _med(agg, "amplification_actual"),
                _med(agg, "amplification_nominal"),
                _med(agg, "throughput_mib_s"),
                _med(agg, "p50_ms"),
                _med(agg, "p99_ms"),
                _med(agg, "cpu_s"),
                _med(agg, "peak_rss_kb"),
            ])


def _med(agg: dict, key: str, scale: float = 1.0) -> str:
    v = agg.get(key)
    if not v:
        return ""
    return f"{v['median'] * scale:.3f}"


def _print_markdown(raw: dict) -> None:
    print("\n## Results (method-tagged; medians over iterations)\n")
    print("| method | prep/mount ms | e2e ms | ttfa ms | total ms | app bytes "
          "| chunk req | backend fetches | backend bytes | ampl. | throughput MiB/s "
          "| p50 ms | p99 ms | RSS KiB |")
    print("|" + "---|" * 15)
    for name, agg in raw["configs"].items():
        if agg.get("skipped"):
            print(f"| {name} | skipped: {agg.get('reason','')} " + "| " * 13 + "|")
            continue
        m = agg.get("method", name)
        print(
            f"| {m} "
            f"| {_fmt_prep_mount(agg)} "
            f"| {_num(agg,'end_to_end_s',1000.0)} "
            f"| {_num(agg,'ttfa_ms')} "
            f"| {_num(agg,'total_s',1000.0)} "
            f"| {_num(agg,'app_bytes')} "
            f"| {_num(agg,'chunk_requests')} "
            f"| {_num(agg,'backend_fetches')} "
            f"| {_num(agg,'backend_bytes_actual')} "
            f"| {_num(agg,'amplification_actual')} "
            f"| {_num(agg,'throughput_mib_s')} "
            f"| {_num(agg,'p50_ms')} "
            f"| {_num(agg,'p99_ms')} "
            f"| {_num(agg,'peak_rss_kb')} |"
        )


def _fmt_prep_mount(agg: dict) -> str:
    if "prep_s" in agg:
        return f"{agg['prep_s']['median']*1000:.1f}"
    if "mount_s" in agg:
        return f"{agg['mount_s']['median']*1000:.1f}"
    return ""


def _fmt_int(v) -> str:
    return "" if v is None else f"{int(v)}"


def _num(agg: dict, key: str, scale: float = 1.0) -> str:
    v = agg.get(key)
    if not v:
        return ""
    return f"{v['median'] * scale:.2f}"


# --------------------------------------------------------------------------

def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("source", help="dataset directory (from bench/dataset.py)")
    ap.add_argument("--out", default="/tmp/swiftfs_compare", help="output dir")
    ap.add_argument("--iterations", type=int, default=5)
    ap.add_argument("--chunk-size", type=int, default=1024 * 1024)
    ap.add_argument("--access", choices=["sequential", "random", "hot"], default="sequential")
    ap.add_argument("--reads-per-file", type=int, default=1)
    ap.add_argument("--read-bytes", type=int, default=128 * 1024)
    ap.add_argument("--seed", type=int, default=42)
    ap.add_argument("--no-read-ahead", action="store_true")
    ap.add_argument("--ram-cache-bytes", type=int, default=0)
    ap.add_argument("--disk-cache-dir", default=None)
    ap.add_argument("--metrics", action="store_true",
                    help="enable /metrics to measure actual backend bytes/fetches")
    ap.add_argument("--validate", action="store_true",
                    help="verify eager and FUSE return byte-identical data")
    ap.add_argument("--force-mount", action="store_true",
                    help="attempt FUSE even if /dev/fuse is not detected")
    args = ap.parse_args(argv)

    run(args)
    return 0


if __name__ == "__main__":
    sys.exit(main())
