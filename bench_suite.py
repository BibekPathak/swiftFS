import csv
#!/usr/bin/env python3
"""Benchmark SwiftFS against eager extraction for various workloads.

This script:
1. Generates (or uses existing) synthetic datasets
2. Creates a normal filesystem via eager extraction (cp -r)
3. Creates a SwiftFS image via `swiftfs build`
4. Measures various performance metrics
5. Stores results as CSV/JSON
6. Generates summary statistics

Requires:
- Python 3
- swiftfs binary (built from this repo)
- Standard Unix tools (cp, time, etc.)
"""

import os
import sys
import json
import random
import subprocess
import time
import shutil
from pathlib import Path
from statistics import mean, median, stdev

# Configuration
SWIFTFS_BIN = "/home/BibekP/swiftFS/target/debug/swiftfs"
SWIFTFS_DATA_DIR = "/tmp/swiftfs_bench_data"
RESULTS_DIR = "/tmp/swiftfs_bench_results"
CHUNK_SIZE = 1024 * 1024  # 1 MiB

# Ensure directories exist
os.makedirs(SWIFTFS_DATA_DIR, exist_ok=True)
os.makedirs(RESULTS_DIR, exist_ok=True)


def run_cmd(cmd, cwd=None, timeout=60):
    """Run a command and return (returncode, stdout, stderr)."""
    try:
        result = subprocess.run(
            cmd, cwd=cwd, capture_output=True, text=True, timeout=timeout
        )
        return result.returncode, result.stdout, result.stderr
    except subprocess.TimeoutExpired:
        return -1, "", "TIMEOUT"


def measure_time(cmd, cwd=None):
    """Measure wall-clock time of a command."""
    start = time.time()
    rc, out, err = run_cmd(cmd, cwd=cwd)
    elapsed = time.time() - start
    return elapsed, rc, out, err


def dataset_path(name):
    """Get path for a dataset."""
    return os.path.join(SWIFTFS_DATA_DIR, name)


def eager_extract_summary(source_dir):
    """Measure eager extraction: just copy and report size."""
    # The "eager" approach just copies the directory
    tmpdir = tempfile_dir()
    t0 = time.time()
    rc, out, err = run_cmd(["cp", "-r", source_dir, tmpdir])
    t1 = time.time()
    size_mb = 0
    if os.path.isdir(tmpdir):
        total = sum(f.stat().st_size for f in Path(tmpdir).rglob('*') if f.is_file())
        size_mb = size_mb / (1024 * 1024)
    shutil.rmtree(tmpdir, ignore_errors=True)
    return {
        "prep_time_s": t1 - t0,
        "size_mb": size_mb,
        "rc": rc,
    }


def swiftfs_build_image(source_dir, image_dir, chunk_size=CHUNK_SIZE):
    """Build a SwiftFS image from a source directory."""
    t0 = time.time()
    rc, out, err = run_cmd([
        SWIFTFS_BIN, "build", source_dir, image_dir,
        "--chunk-size", str(chunk_size)
    ])
    t1 = time.time()
    return {
        "prep_time_s": t1 - t0,
        "rc": rc,
        "build_out": out,
        "build_err": err,
    }


def swiftfs_inspect(image_dir):
    """Inspect a SwiftFS image."""
    t0 = time.time()
    rc, out, err = run_cmd([SWIFTFS_BIN, "inspect", image_dir])
    t1 = time.time()
    return {
        "inspect_time_s": t1 - t0,
        "rc": rc,
        "inspect_out": out,
        "inspect_err": err,
    }


def swiftfs_verify(image_dir):
    """Verify a SwiftFS image."""
    t0 = time.time()
    rc, out, err = run_cmd([SWIFTFS_BIN, "verify", image_dir])
    t1 = time.time()
    return {
        "verify_time_s": t1 - t0,
        "rc": rc,
        "verify_out": out,
        "verify_err": err,
    }


def swiftfs_mount(image_dir, mount_point):
    """Mount a SwiftFS image."""
    # Unmount if already mounted
    run_cmd(["fusermount3", "-u", mount_point], timeout=5)
    t0 = time.time()
    rc, out, err = run_cmd([SWIFTFS_BIN, "mount", image_dir, mount_point])
    t1 = time.time()
    mounted = rc == 0
    return {
        "mount_time_s": t1 - t0,
        "rc": rc,
        "mounted": mounted,
        "mount_out": out,
        "mount_err": err,
    }


def swiftfs_unmount(mount_point):
    """Unmount a SwiftFS image."""
    t0 = time.time()
    rc, out, err = run_cmd(["fusermount3", "-u", mount_point])
    t1 = time.time()
    return {
        "unmount_time_s": t1 - t0,
        "rc": rc,
    }


def swiftfs_read_file(image_dir, mount_point, filename, num_bytes=8192):
    """Read a file from the mounted SwiftFS image."""
    fpath = os.path.join(mount_point, filename)
    t0 = time.time()
    rc, out, err = run_cmd(["dd", "if="+fpath, "bs=1", f"count={num_bytes}"], timeout=10)
    t1 = time.time()
    return {
        "read_time_s": t1 - t0,
        "data_len": len(out) if out else 0,
        "rc": rc,
    }


def count_files(directory):
    """Count the number of files in a directory tree."""
    return len([f for f in Path(directory).rglob('*') if f.is_file()])


def dataset_size_mb(directory):
    """Get total size of a dataset in MB."""
    total = sum(f.stat().st_size for f in Path(directory).rglob('*') if f.is_file())
    return total / (1024 * 1024)


def benchmark_workload(name, source_dir, iterations=3):
    """Run a complete benchmark suite for a workload."""
    results = []
    
    print(f"\n=== Benchmark: {name} ===")
    print(f"Source: {source_dir}")
    
    # 1. Eager extraction timing
    print("  --- Eager extraction ---")
    eager = eager_extract_summary(source_dir)
    results.append(("eager_prep", eager))
    
    # 2. SwiftFS build
    print("  --- SwiftFS build ---")
    ds_name = os.path.basename(source_dir)
    sf_result = swiftfs_build_image(source_dir, dataset_path(ds_name))
    results.append(("swiftfs_build", sf_result))
    
    # 3. SwiftFS inspect
    print("  --- SwiftFS inspect ---")
    insp = swiftfs_inspect(dataset_path(ds_name))
    results.append(("swiftfs_inspect", insp))
    
    # 4. SwiftFS verify
    print("  --- SwiftFS verify ---")
    vrfy = swiftfs_verify(dataset_path(ds_name))
    results.append(("swiftfs_verify", vrfy))
    
    # 5. Mount and read benchmarks
    mount_pt = tempfile_dir()
    if os.path.exists(mount_pt):
        # Check if already mounted
        mounted = False
    else:
        mounted = False
    
    try:
        if not mounted:
            mnt = swiftfs_mount(dataset_path(ds_name), mount_pt)
            if mnt["mounted"]:
                # 6. Read benchmarks
                print("  --- Read benchmarks (mounted) ---")
                
                # Read small file
                small_read = swiftfs_read_file(dataset_path(ds_name), mount_pt, 
                                              "file_00001.bin", num_bytes=128)
                results.append(("small_read", small_read))
                
                # Read large file (partial)
                large_read = swiftfs_read_file(dataset_path(ds_name), mount_pt, 
                                              "large.bin", num_bytes=8192)
                results.append(("large_read", large_read))
                
                # 7. Unmount
                un = swiftfs_unmount(mount_pt)
                results.append(("unmount", un))
            else:
                print(f"  Mount failed: {mnt.get('mount_err', 'unknown')}")
    finally:
        if os.path.exists(mount_pt):
            shutil.rmtree(mount_pt, ignore_errors=True)
    
    return results


def tempfile_dir():
    """Create a temporary directory."""
    import tempfile
    td = tempfile.mkdtemp(prefix="swiftfs_bench_")
    return td


def main():
    if len(sys.argv) < 2:
        print("Usage: python3 bench_suite.py <dataset_name> [iterations]")
        print(f"Available datasets in {SWIFTFS_DATA_DIR}:")
        if os.path.exists(SWIFTFS_DATA_DIR):
            for d in os.listdir(SWIFTFS_DATA_DIR):
                if os.path.isdir(os.path.join(SWIFTFS_DATA_DIR, d)):
                    print(f"  {d}")
        sys.exit(1)
    
    dataset_name = sys.argv[1]
    iterations = int(sys.argv[2]) if len(sys.argv) > 2 else 3
    
    ds_dir = dataset_path(dataset_name)
    if not os.path.isdir(ds_dir):
        print(f"Dataset {dataset_name} not found. Generate datasets first.")
        print(f"Run: python3 {os.path.basename(__file__).replace('.py', '')} gen_datasets.py <output_dir>")
        sys.exit(1)
    
    print(f"Running benchmarks for dataset: {dataset_name}")
    print(f"Dataset size: {dataset_size_mb(ds_dir):.2f} MB")
    
    # Run the benchmark suite
    results = benchmark_workload(dataset_name, ds_dir, iterations=iterations)
    
    # Store results
    result_file = os.path.join(RESULTS_DIR, f"{dataset_name}_results.json")
    with open(result_file, 'w') as f:
        json.dump({k: v for k, v in results}, f, indent=2)
    
    # Also store as CSV-like summary
    csv_file = os.path.join(RESULTS_DIR, f"{dataset_name}_summary.csv")
    with open(csv_file, 'w', newline='') as f:
        writer = csv.writer(f)
        writer.writerow(["benchmark", "prep_time_s", "rc", "details"])
        for name, r in results:
            writer.writerow([name, r["prep_time_s"], r["rc"], r.get("build_err", "")[:50]])
    
    print(f"\nResults stored:")
    print(f"  JSON: {result_file}")
    print(f"  CSV:  {csv_file}")
    
    # Print summary
    print(f"\nSummary for {dataset_name}:")
    for name, r in results:
        print(f"  {name}: prep={r['prep_time_s']:.3f}s, rc={r['rc']}")


if __name__ == '__main__':
    if len(sys.argv) > 1 and sys.argv[1] == 'gen':
        # Generate datasets
        out_dir = sys.argv[2] if len(sys.argv) > 2 else SWIFTFS_DATA_DIR
        # Use default params for quick generation
        import subprocess
        # Just generate small files and a large file
        os.makedirs(out_dir, exist_ok=True)
        # Many small files
        for i in range(100):
            data = os.urandom(random.randint(1, 4096))
            with open(os.path.join(out_dir, f"file_{i:05d}.bin"), 'wb') as f:
                f.write(data)
        # Large file
        data = os.urandom(5 * 1024 * 1024)  # 5 MB
        with open(os.path.join(out_dir, "large.bin"), 'wb') as f:
            f.write(data)
        # Shard files
        for i in range(5):
            data = os.urandom((i+1) * 256 * 1024)  # 256KB - 2MB
            with open(os.path.join(out_dir, f"shard_{i:03d}.bin"), 'wb') as f:
                f.write(data)
        # Hot + cold
        with open(os.path.join(out_dir, "model.weights"), 'wb') as f:
            f.write(os.urandom(2 * 1024 * 1024))  # 2 MB
        for i in range(10):
            with open(os.path.join(out_dir, f"cold_{i:03d}.bin"), 'wb') as f:
                f.write(os.urandom(random.randint(1, 4096)))
        print(f"Generated test dataset in {out_dir}")
    else:
        main()