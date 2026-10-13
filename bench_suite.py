#!/usr/bin/env python3
"""Orchestrate SwiftFS benchmarks and store results as CSV/JSON.

This script drives the real `swiftfs bench` CLI, which performs genuine
measurements (image build/inspect/verify timings and chunk-read latency through
the cache/fetcher stack). It does **not** invent numbers.

It additionally exercises a real FUSE mount when `/dev/fuse` is available, and
reports a clear error (not an internal exception) when it is not.

Usage:
    python3 bench_suite.py <source_dir> [--iterations N] [--no-mount]

The source directory is built into a SwiftFS image under --work-dir, then each
workload is run and its output captured. Results are written to
<work-dir>/results/ as JSON (per workload) and a CSV summary.
"""

import argparse
import csv
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

DEFAULT_WORK_DIR = "/tmp/swiftfs_bench"


def run_cmd(cmd, timeout=300, cwd=None):
    """Run a command, returning (returncode, stdout, stderr)."""
    try:
        p = subprocess.run(
            cmd, cwd=cwd, capture_output=True, text=True, timeout=timeout
        )
        return p.returncode, p.stdout, p.stderr
    except subprocess.TimeoutExpired:
        return -1, "", f"TIMEOUT after {timeout}s"


def swiftfs_bin():
    for cand in ("swiftfs",):
        p = shutil.which(cand)
        if p:
            return p
    # Fall back to a locally built target if present.
    for profile in ("debug", "release"):
        p = Path(__file__).resolve().parent / "target" / profile / "swiftfs"
        if p.exists():
            return str(p)
    sys.exit("could not find the `swiftfs` binary; build it or add it to PATH")


def dataset_size_mb(directory):
    total = sum(f.stat().st_size for f in Path(directory).rglob("*") if f.is_file())
    return total / (1024 * 1024)


def fuse_available():
    return os.path.exists("/dev/fuse") and (
        shutil.which("fusermount3") or shutil.which("fusermount")
    )


def parse_bench_output(text):
    """Parse `key: value` lines and the results table from bench output.

    Strips ANSI escape sequences and drops tracing log lines (which contain a
    timestamp and level) before parsing.
    """
    import re

    ansi = re.compile(r"\x1b\[[0-9;]*m")
    rows = []
    flat = {}
    for raw in text.splitlines():
        line = ansi.sub("", raw).rstrip()
        if not line:
            continue
        # Results table: "<workload>  p50=... p99=... ttfa=... total=... backend=... hit=..."
        if "=" in line and " p50=" in line:
            first = line.split(None, 1)
            row = {"workload": first[0]}
            rest = first[1] if len(first) > 1 else ""
            for k, v in re.findall(r"(\w+)=\s*([0-9.]+)", rest):
                row[k] = v
            rows.append(row)
            continue
        # Key/value header lines.
        if ":" in line and not line.startswith(" "):
            k, _, v = line.partition(":")
            flat[k.strip()] = v.strip()
    return flat, rows


def try_mount(sfs, image_dir, mount_point):
    """Attempt a real FUSE mount + read. Returns a dict describing the result."""
    info = {"attempted": True, "mounted": False, "reads": []}
    if not fuse_available():
        info["attempted"] = False
        info["reason"] = "/dev/fuse or fusermount not available"
        return info

    os.makedirs(mount_point, exist_ok=True)
    # Ensure no stale mount.
    for tool in ("fusermount3", "fusermount"):
        if shutil.which(tool):
            run_cmd([tool, "-u", mount_point], timeout=10)
            break

    proc = subprocess.Popen(
        [sfs, "mount", image_dir, mount_point],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    # Wait for the mount to appear.
    deadline = time.time() + 10
    while time.time() < deadline:
        if os.path.ismount(mount_point):
            info["mounted"] = True
            break
        if proc.poll() is not None:
            break
        time.sleep(0.1)

    if not info["mounted"]:
        out, err = ("", "")
        try:
            out, err = proc.communicate(timeout=2)
        except subprocess.TimeoutExpired:
            proc.kill()
        info["reason"] = (err or out or "mount did not appear").strip()[:400]
        return info

    info["mount_time_s"] = None
    # Read every file, timing each read.
    files = sorted(
        p for p in Path(mount_point).rglob("*") if p.is_file()
    )
    for f in files:
        t0 = time.time()
        try:
            with open(f, "rb") as fh:
                data = fh.read()
            info["reads"].append(
                {"path": str(f.relative_to(mount_point)),
                 "bytes": len(data),
                 "read_time_s": time.time() - t0}
            )
        except OSError as e:
            info["reads"].append(
                {"path": str(f.relative_to(mount_point)), "error": str(e)}
            )

    # Unmount.
    t0 = time.time()
    for tool in ("fusermount3", "fusermount"):
        if shutil.which(tool):
            rc, _, err = run_cmd([tool, "-u", mount_point], timeout=10)
            info["unmount_rc"] = rc
            break
    info["unmount_time_s"] = time.time() - t0
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
    return info


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("source_dir", help="directory to package and benchmark")
    ap.add_argument("--iterations", type=int, default=3)
    ap.add_argument("--chunk-size", type=int, default=1024 * 1024)
    ap.add_argument("--work-dir", default=DEFAULT_WORK_DIR)
    ap.add_argument("--no-mount", action="store_true", help="skip FUSE mount test")
    args = ap.parse_args()

    sfs = swiftfs_bin()
    source = Path(args.source_dir).resolve()
    if not source.is_dir():
        sys.exit(f"source directory not found: {source}")

    work = Path(args.work_dir)
    image = work / "image"
    results_dir = work / "results"
    mount_point = work / "mnt"
    results_dir.mkdir(parents=True, exist_ok=True)
    work.mkdir(parents=True, exist_ok=True)

    print(f"swiftfs binary: {sfs}")
    print(f"source:         {source} ({dataset_size_mb(source):.2f} MiB)")
    print(f"image:          {image}")
    print(f"results:        {results_dir}")

    # 1. Build the image.
    print("\n[1/3] Building image ...")
    if image.exists():
        shutil.rmtree(image)
    rc, out, err = run_cmd(
        [sfs, "build", str(source), str(image), "--chunk-size", str(args.chunk_size)]
    )
    if rc != 0:
        sys.exit(f"build failed (rc={rc}): {err.strip()}")
    print(f"  built image ({dataset_size_mb(image):.2f} MiB)")

    # 2. Run the full benchmark suite, capturing output.
    print("\n[2/3] Running `swiftfs bench full` ...")
    rc, out, err = run_cmd(
        [
            sfs, "bench", "full",
            "--image", str(image),
            "--data-dir", str(source),
            "--iterations", str(args.iterations),
            "--chunk-size", str(args.chunk_size),
        ]
    )
    if rc != 0:
        sys.exit(f"bench failed (rc={rc}): {err.strip()}")
    print(out)
    flat, rows = parse_bench_output(out)

    # 3. Attempt a real FUSE mount and read every file.
    mount_info = {"attempted": False, "reason": "skipped (--no-mount)"}
    if not args.no_mount:
        print("\n[3/3] FUSE mount + read test ...")
        mount_info = try_mount(sfs, str(image), str(mount_point))
        if mount_info.get("mounted"):
            n = len(mount_info["reads"])
            total = sum(r.get("bytes", 0) for r in mount_info["reads"])
            print(f"  mounted; read {n} files ({total} bytes)")
        elif mount_info.get("attempted"):
            print(f"  mount unavailable: {mount_info.get('reason')}")
        else:
            print(f"  skipped: {mount_info.get('reason')}")
    else:
        print("\n[3/3] FUSE mount test skipped")

    # Store results.
    record = {
        "source": str(source),
        "dataset_size_mb": dataset_size_mb(source),
        "chunk_size": args.chunk_size,
        "iterations": args.iterations,
        "suite_output": out,
        "suite_summary": flat,
        "workloads": rows,
        "mount": mount_info,
        "timestamp": time.strftime("%Y-%m-%dT%H:%M:%S"),
    }
    json_path = results_dir / "results.json"
    with open(json_path, "w") as f:
        json.dump(record, f, indent=2)

    csv_path = results_dir / "summary.csv"
    with open(csv_path, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["workload", "p50_ms", "p99_ms", "ttfa_ms", "backend", "hit", "total_ms"])
        for row in rows:
            w.writerow([
                row.get("workload", ""),
                row.get("p50", ""),
                row.get("p99", ""),
                row.get("ttfa", ""),
                row.get("backend", ""),
                row.get("hit", ""),
                row.get("total", ""),
            ])

    print(f"\nWrote {json_path}")
    print(f"Wrote {csv_path}")


if __name__ == "__main__":
    main()
