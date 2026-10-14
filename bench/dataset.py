#!/usr/bin/env python3
"""Deterministic, configurable synthetic dataset generator for SwiftFS benchmarks.

A dataset is a directory tree plus a JSON *manifest* describing exactly how it
was built, so any result can be traced back to a reproducible input.

Configuration knobs (all reproducible from a seed):

- ``total_bytes``: approximate total logical size.
- ``file_count``: number of files (when a distribution is used, sizes are drawn
  to approximate ``total_bytes``).
- ``size_distribution``: ``fixed`` | ``uniform`` | ``pareto``.
- ``min_bytes`` / ``max_bytes``: bounds for ``uniform``.
- ``layout``: ``flat`` | ``dirs`` (sharded subdirectories).
- ``hot_fraction``: fraction of files designated "hot" (read first in the
  application-ready workload); recorded, not enforced here.
- ``seed``: RNG seed.

This module is dependency-free (standard library only).
"""

from __future__ import annotations

import json
import os
import random
from dataclasses import dataclass, field, asdict
from pathlib import Path
from typing import Optional


@dataclass
class DatasetConfig:
    """Parameters for dataset generation."""

    name: str = "default"
    total_bytes: int = 64 * 1024 * 1024
    file_count: int = 128
    size_distribution: str = "uniform"  # fixed | uniform | pareto
    min_bytes: int = 1024
    max_bytes: int = 4 * 1024 * 1024
    fixed_bytes: int = 1024 * 1024
    pareto_alpha: float = 1.5
    layout: str = "dirs"  # flat | dirs
    dirs: int = 4
    hot_fraction: float = 0.1
    seed: int = 42

    @staticmethod
    def from_dict(d: dict) -> "DatasetConfig":
        fields = {f for f in DatasetConfig.__dataclass_fields__}  # type: ignore[attr-defined]
        unknown = set(d) - fields
        if unknown:
            raise ValueError(f"unknown dataset config keys: {sorted(unknown)}")
        return DatasetConfig(**d)

    @staticmethod
    def load(path: str | os.PathLike) -> "DatasetConfig":
        with open(path) as f:
            return DatasetConfig.from_dict(json.load(f))


def _draw_sizes(cfg: DatasetConfig, rng: random.Random) -> list[int]:
    """Draw ``file_count`` file sizes according to the configured distribution."""
    n = max(1, cfg.file_count)
    if cfg.size_distribution == "fixed":
        return [cfg.fixed_bytes] * n
    if cfg.size_distribution == "uniform":
        lo = max(1, cfg.min_bytes)
        hi = max(lo, cfg.max_bytes)
        return [rng.randint(lo, hi) for _ in range(n)]
    if cfg.size_distribution == "pareto":
        # Pareto-ish: a few large files dominate. Scale so the mean ≈ target.
        lo = max(1, cfg.min_bytes)
        raw = [(rng.paretovariate(cfg.pareto_alpha)) for _ in range(n)]
        mean = sum(raw) / n
        target_mean = max(lo, cfg.total_bytes // n)
        sizes = [max(lo, int(r * target_mean / mean)) for r in raw]
        return sizes
    raise ValueError(f"unknown size_distribution: {cfg.size_distribution!r}")


def generate(cfg: DatasetConfig, root: str | os.PathLike) -> dict:
    """Create the dataset under ``root`` and return its manifest as a dict."""
    root = Path(root)
    if root.exists():
        # Required for reproducibility: refuse to merge into an existing tree.
        raise FileExistsError(f"dataset root already exists: {root}")
    root.mkdir(parents=True)

    rng = random.Random(cfg.seed)
    sizes = _draw_sizes(cfg, rng)
    rng.shuffle(sizes)

    n_hot = max(0, int(round(cfg.file_count * cfg.hot_fraction)))
    hot_indices = set(rng.sample(range(len(sizes)), min(n_hot, len(sizes))))

    files: list[dict] = []
    for i, size in enumerate(sizes):
        if cfg.layout == "dirs":
            sub = f"d{i % max(1, cfg.dirs):03d}"
            rel = f"{sub}/f{i:05d}.bin"
        else:
            rel = f"f{i:05d}.bin"
        path = root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        # Deterministic pseudo-random content without holding the whole file.
        _write_pseudorandom(path, size, seed=cfg.seed * 1_000_003 + i)
        files.append({"path": rel, "bytes": size, "hot": i in hot_indices})

    total = sum(f["bytes"] for f in files)
    manifest = {
        "config": asdict(cfg),
        "root": str(root),
        "file_count": len(files),
        "total_bytes": total,
        "hot_files": sorted(f["path"] for f in files if f["hot"]),
        "files": files,
    }
    with open(root / "dataset_manifest.json", "w") as f:
        json.dump(manifest, f, indent=2)
    return manifest


def _write_pseudorandom(path: Path, size: int, seed: int) -> None:
    """Write ``size`` bytes of incompressible-looking but reproducible content."""
    # Use a per-file RNG to avoid materialising the whole file in memory.
    chunk = 1 << 20
    written = 0
    # Seed a cheap xorshift from the file seed for speed.
    state = seed & 0xFFFFFFFF or 0x1234_5678
    buf = bytearray(chunk)
    with open(path, "wb") as f:
        while written < size:
            n = min(chunk, size - written)
            for j in range(0, n, 4):
                state ^= (state << 13) & 0xFFFFFFFF
                state ^= state >> 17
                state ^= (state << 5) & 0xFFFFFFFF
                buf[j : j + 4] = state.to_bytes(4, "little")
            f.write(bytes(buf[:n]))
            written += n


def load_manifest(root: str | os.PathLike) -> dict:
    with open(Path(root) / "dataset_manifest.json") as f:
        return json.load(f)


if __name__ == "__main__":
    import argparse

    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("root", help="directory to create (must not exist)")
    ap.add_argument("--config", help="JSON config file")
    ap.add_argument("--total-mb", type=float, default=None)
    ap.add_argument("--file-count", type=int, default=None)
    ap.add_argument("--distribution", choices=["fixed", "uniform", "pareto"], default=None)
    ap.add_argument("--min-bytes", type=int, default=None)
    ap.add_argument("--max-bytes", type=int, default=None)
    ap.add_argument("--layout", choices=["flat", "dirs"], default=None)
    ap.add_argument("--seed", type=int, default=None)
    args = ap.parse_args()

    cfg = DatasetConfig.load(args.config) if args.config else DatasetConfig()
    if args.total_mb is not None:
        cfg.total_bytes = int(args.total_mb * 1024 * 1024)
    if args.file_count is not None:
        cfg.file_count = args.file_count
    if args.distribution is not None:
        cfg.size_distribution = args.distribution
    if args.min_bytes is not None:
        cfg.min_bytes = args.min_bytes
    if args.max_bytes is not None:
        cfg.max_bytes = args.max_bytes
    if args.layout is not None:
        cfg.layout = args.layout
    if args.seed is not None:
        cfg.seed = args.seed

    manifest = generate(cfg, args.root)
    print(
        f"generated {manifest['file_count']} files, "
        f"{manifest['total_bytes']} bytes ({manifest['total_bytes']/1048576:.2f} MiB) "
        f"at {args.root}"
    )
