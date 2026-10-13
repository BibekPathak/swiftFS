#!/usr/bin/env python3
"""Phase 7: Run SwiftFS benchmarks and generate performance report.

Orchestrates benchmark runs using the SwiftFS CLI. Generates datasets,
builds SwiftFS images, runs CLI benchmarks, and stores results as CSV/JSON.

Note: Actual FUSE-mounted benchmarks require a working FUSE environment
with appropriate permissions. This script focuses on the orchestration
framework and collects metrics available via the CLI.
"""

import os
import sys
import json
import subprocess
import csv
import time
import shutil
from pathlib import Path

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


def dataset_path(name):
    """Get path for a dataset."""
    return os.path.join(SWIFTFS_DATA_DIR, name)


def swiftfs_build_image(source_dir, image_dir, chunk_size=CHUNK_SIZE):
    """Build a SwiftFS image from a source directory."""
    # Clean output directory if it exists
    if os.path.exists(image_dir):
        shutil.rmtree(image_dir, ignore_errors=True)
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


def count_files(directory):
    """Count the number of files in a directory tree."""
    return len([f for f in Path(directory).rglob('*') if f.is_file()])


def dataset_size_mb(directory):
    """Get total size of a dataset in MB."""
    total = sum(f.stat().st_size for f in Path(directory).rglob('*') if f.is_file())
    return total / (1024 * 1024)


def main():
    if len(sys.argv) < 2:
        print("Usage: python3 bench_suite.py <dataset_name>")
        print(f"Available datasets in {SWIFTFS_DATA_DIR}:")
        if os.path.exists(SWIFTFS_DATA_DIR):
            for d in os.listdir(SWIFTFS_DATA_DIR):
                if os.path.isdir(os.path.join(SWIFTFS_DATA_DIR, d)):
                    print(f"  {d}")
        sys.exit(1)
    
    dataset_name = sys.argv[1]
    ds_dir = dataset_path(dataset_name)
    
    if not os.path.isdir(ds_dir):
        print(f"Dataset {dataset_name} not found. Generating...")
        import random
        os.makedirs(ds_dir, exist_ok=True)
        # Many small files
        for i in range(20):
            data = os.urandom(random.randint(1, 4096))
            with open(os.path.join(ds_dir, f"file_{i:05d}.bin"), 'wb') as f:
                f.write(data)
        # Large file
        data = os.urandom(5 * 1024 * 1024)
        with open(os.path.join(ds_dir, "large.bin"), 'wb') as f:
            f.write(data)
        # Shard files
        for i in range(5):
            data = os.urandom((i+1) * 256 * 1024)
            with open(os.path.join(ds_dir, f"shard_{i:03d}.bin"), 'wb') as f:
                f.write(data)
        # Hot + cold files
        with open(os.path.join(ds_dir, "model.weights"), 'wb') as f:
            f.write(os.urandom(2 * 1024 * 1024))
        for i in range(10):
            with open(os.path.join(ds_dir, f"cold_{i:03d}.bin"), 'wb') as f:
                f.write(os.urandom(random.randint(1, 4096)))
    
    print(f"Benchmark suite for dataset: {dataset_name}")
    print(f"Dataset size: {dataset_size_mb(ds_dir):.2f} MB")
    
    results = {}
    
    # Step 1: Build SwiftFS image
    print("\n1. Building SwiftFS image...")
    build_result = swiftfs_build_image(ds_dir, dataset_path(f"{dataset_name}_image"))
    results["build"] = build_result
    print(f"   Build time: {build_result['prep_time_s']:.3f}s, rc={build_result['rc']}")
    if build_result['rc'] != 0:
        print(f"   Error: {build_result['build_err'][:200]}")
    
    # Step 2: Inspect
    print("\n2. Inspecting image...")
    insp = swiftfs_inspect(dataset_path(f"{dataset_name}_image"))
    results["inspect"] = insp
    print(f"   Inspect time: {insp['inspect_time_s']:.3f}s, rc={insp['rc']}")
    if insp['rc'] != 0:
        print(f"   Error: {insp['inspect_err'][:200]}")
    
    # Step 3: Verify
    print("\n3. Verifying image...")
    vrfy = swiftfs_verify(dataset_path(f"{dataset_name}_image"))
    results["verify"] = vrfy
    print(f"   Verify time: {vrfy['verify_time_s']:.3f}s, rc={vrfy['rc']}")
    if vrfy['rc'] != 0:
        print(f"   Error: {vrfy['verify_err'][:200]}")
    
    # Step 4: Run bench CLI
    print("\n4. Running bench CLI...")
    bench_out = ""
    bench_err = ""
    try:
        rc, out, err = run_cmd([SWIFTFS_BIN, "bench", "read", dataset_name])
        bench_out = out
        bench_err = err
    except Exception as e:
        bench_err = str(e)[:200]
    results["bench_cli"] = {
        "rc": rc if 'rc' in dir() else -1,
        "out": bench_out,
        "err": bench_err,
    }
    print(f"   Bench CLI rc={results['bench_cli']['rc']}")
    if bench_out:
        print(f"   Output: {bench_out[:300]}")
    if bench_err:
        print(f"   Error: {bench_err[:200]}")
    
    # Step 5: Store results
    dataset_size = dataset_size_mb(ds_dir)
    result_data = {
        "dataset": dataset_name,
        "dataset_size_mb": dataset_size,
        "chunk_size": CHUNK_SIZE,
        "results": results,
    }
    
    # Store as JSON
    result_file = os.path.join(RESULTS_DIR, f"{dataset_name}_results.json")
    with open(result_file, 'w') as f:
        json.dump(result_data, f, indent=2)
    
    # Store as CSV summary
    csv_file = os.path.join(RESULTS_DIR, f"{dataset_name}_summary.csv")
    with open(csv_file, 'w', newline='') as f:
        writer = csv.writer(f)
        writer.writerow(["stage", "time_s", "rc", "details"])
        writer.writerow(["build", build_result["prep_time_s"], build_result["rc"], 
                        build_result["build_err"][:50] if build_result["build_err"] else ""])
        writer.writerow(["inspect", insp["inspect_time_s"], insp["rc"], ""])
        writer.writerow(["verify", vrfy["verify_time_s"], vrfy["rc"], ""])
        writer.writerow(["bench_cli", results["bench_cli"]["rc"], results["bench_cli"]["rc"], ""])
    
    print(f"\nResults stored:")
    print(f"  JSON: {result_file}")
    print(f"  CSV:  {csv_file}")
    
    # Print summary
    print(f"\nSummary for {dataset_name}:")
    print(f"  Dataset size: {dataset_size:.2f} MB")
    print(f"  Build: {build_result['prep_time_s']:.3f}s (rc={build_result['rc']})")
    print(f"  Inspect: {insp['inspect_time_s']:.3f}s (rc={insp['rc']})")
    print(f"  Verify: {vrfy['verify_time_s']:.3f}s (rc={vrfy['rc']})")
    print(f"  Bench CLI: rc={results['bench_cli']['rc']}")
    
    return result_data


if __name__ == '__main__':
    main()