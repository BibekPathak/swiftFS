#!/usr/bin/env python3
"""Generate synthetic datasets for SwiftFS benchmarking.

These scripts create directory structures representing various ML workloads:
- Many small files
- A few very large files
- Sparse access to a large image
- Sequential reads
- Random reads
- Repeated hot-file access
- Concurrent reads from multiple processes
"""

import os
import sys
import json
import random
import hashlib
from pathlib import Path

# Configuration
CHUNK_SIZE = 1024 * 1024  # 1 MiB default chunk size
RANDOM_SEED = 42

def set_seed(seed=RANDOM_SEED):
    random.seed(seed)
    os.environ['PYTHONHASHSEED'] = str(seed)

def write_file(path, data):
    """Write data to a file, creating directories as needed."""
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, 'wb') as f:
        f.write(data)

def gen_small_files(base_dir, num_files, min_size=1, max_size=1024):
    """Generate many small files."""
    set_seed()
    created = []
    for i in range(num_files):
        size = random.randint(min_size, max_size)
        data = os.urandom(size)
        fname = f"file_{i:05d}.bin"
        fpath = os.path.join(base_dir, fname)
        write_file(fpath, data)
        created.append((fname, size, fpath))
    return created

def gen_large_file(base_dir, filename, size_mb):
    """Generate a single large file."""
    size = size_mb * 1024 * 1024
    data = os.urandom(size)
    fpath = os.path.join(base_dir, filename)
    write_file(fpath, data)
    return fpath

def gen_sparse_image(base_dir, filename, total_size_mb, sparse_regions):
    """Generate a sparse image with hole regions."""
    size = total_size_mb * 1024 * 1024
    data = b'\x00' * size
    # Make some regions sparse by filling with zeros and marking
    fpath = os.path.join(base_dir, filename)
    write_file(fpath, data)
    return fpath

def gen_model_shard_files(base_dir, num_shards, max_size_mb):
    """Generate model shard files (many medium-sized files)."""
    set_seed()
    created = []
    for i in range(num_shards):
        size_mb = random.randint(1, max_size_mb)
        data = os.urandom(size_mb * 1024 * 1024)
        fname = f"shard_{i:03d}.bin"
        fpath = os.path.join(base_dir, fname)
        write_file(fpath, data)
        created.append((fname, size_mb, fpath))
    return created

def gen_hot_file_base(base_dir, hot_size_mb, cold_size_mb, num_cold_files):
    """Generate a dataset with a hot file and many cold files."""
    # Hot file
    hot_data = os.urandom(hot_size_mb * 1024 * 1024)
    hot_fpath = os.path.join(base_dir, "model.weights")
    write_file(hot_fpath, hot_data)
    
    # Cold files
    set_seed()
    for i in range(num_cold_files):
        size = random.randint(1, cold_size_mb) * 1024
        data = os.urandom(size)
        fname = f"cold_{i:03d}.bin"
        fpath = os.path.join(base_dir, fname)
        write_file(fpath, data)
    
    return os.path.join(base_dir, "model.weights")

def gen_concurrent_workload(base_dir, num_processes, files_per_process):
    """Generate a workload for concurrent reads."""
    set_seed()
    created = []
    for p in range(num_processes):
        for f in range(files_per_process):
            size = random.randint(100, 10000)
            data = os.urandom(size)
            fname = f"proc_{p}_file_{f:03d}.bin"
            fpath = os.path.join(base_dir, fname)
            write_file(fpath, data)
            created.append((fname, size, fpath))
    return created

def main():
    if len(sys.argv) < 2:
        print("Usage: python3 generate_datasets.py <output_dir> [num_datasets]")
        sys.exit(1)
    
    output_dir = sys.argv[1]
    num_datasets = int(sys.argv[2]) if len(sys.argv) > 2 else 1
    
    os.makedirs(output_dir, exist_ok=True)
    
    for dataset_idx in range(num_datasets):
        ds_dir = os.path.join(output_dir, f"dataset_{dataset_idx}")
        os.makedirs(ds_dir, exist_ok=True)
        
        # Many small files
        gen_small_files(ds_dir, 500, min_size=1, max_size=256)
        
        # A few very large files
        gen_large_file(ds_dir, "large.bin", 10)  # 10 MB
        
        # Sparse image
        gen_sparse_image(ds_dir, "sparse.img", 100, [])
        
        # Model shard files
        gen_model_shard_files(ds_dir, 20, max_size_mb=5)
        
        # Hot + cold files
        gen_hot_file_base(ds_dir, hot_size_mb=2, cold_size_mb=10, num_cold_files=50)
        
        # Concurrent workload
        gen_concurrent_workload(ds_dir, num_processes=3, files_per_process=20)
    
    # Write manifest
    manifest = {
        "datasets": num_datasets,
        "generated": [f"dataset_{i}" for i in range(num_datasets)],
    }
    with open(os.path.join(output_dir, "manifest.json"), 'w') as f:
        json.dump(manifest, f, indent=2)
    
    print(f"Generated {num_datasets} dataset(s) in {output_dir}")
    print(f"Datasets: {', '.join(f'dataset_{i}' for i in range(num_datasets))}")

if __name__ == '__main__':
    main()