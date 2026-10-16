#!/usr/bin/env python3
"""Shared helpers for the SwiftFS benchmark harness (standard library only)."""

from __future__ import annotations

import json
import os
import platform
import shutil
import statistics
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Optional


# --------------------------------------------------------------------------
# Process / environment helpers
# --------------------------------------------------------------------------

def run_cmd(cmd: list[str], timeout: int = 600, cwd: Optional[str] = None):
    """Run a command, returning (returncode, stdout, stderr)."""
    try:
        p = subprocess.run(
            cmd, cwd=cwd, capture_output=True, text=True, timeout=timeout
        )
        return p.returncode, p.stdout, p.stderr
    except subprocess.TimeoutExpired:
        return -1, "", f"TIMEOUT after {timeout}s"


def swiftfs_bin(prefer_release: bool = True) -> str:
    """Locate the ``swiftfs`` binary, preferring a release build."""
    onpath = shutil.which("swiftfs")
    if onpath:
        return onpath
    here = Path(__file__).resolve().parent.parent
    order = ["release", "debug"] if prefer_release else ["debug", "release"]
    for profile in order:
        p = here / "target" / profile / "swiftfs"
        if p.exists():
            return str(p)
    raise SystemExit("could not find `swiftfs`; build it or add it to PATH")


def fuse_available() -> bool:
    return os.path.exists("/dev/fuse") and (
        shutil.which("fusermount3") or shutil.which("fusermount")
    )


def unmount(mount_point: str) -> None:
    for tool in ("fusermount3", "fusermount"):
        if shutil.which(tool):
            run_cmd([tool, "-u", mount_point], timeout=10)
            return
    run_cmd(["umount", mount_point], timeout=10)


def _read_field(path: str, key: str) -> Optional[str]:
    try:
        with open(path) as f:
            for line in f:
                if line.startswith(key + ":"):
                    return line.split(":", 1)[1].strip()
    except OSError:
        return None
    return None


def process_peak_rss_kb(pid: int) -> Optional[int]:
    """Peak RSS (VmHWM) of a live/recent process, in KiB, or None if unknown."""
    val = _read_field(f"/proc/{pid}/status", "VmHWM")
    if val is None:
        return None
    try:
        return int(val.split()[0])
    except (ValueError, IndexError):
        return None


def process_cpu_time_s(pid: int) -> Optional[tuple[float, float]]:
    """(user_seconds, system_seconds) from /proc/<pid>/stat, or None."""
    try:
        with open(f"/proc/{pid}/stat") as f:
            parts = f.read().split()
    except OSError:
        return None
    # Fields 14 and 15 (1-indexed) are utime and stime in clock ticks.
    try:
        hz = os.sysconf("SC_CLK_TCK")
        utime = int(parts[13]) / hz
        stime = int(parts[14]) / hz
        return (utime, stime)
    except (ValueError, IndexError, OSError):
        return None


def environment_manifest() -> dict:
    """Machine-readable description of the measurement environment."""
    def kv(cmd: list[str]) -> str:
        rc, out, _ = run_cmd(cmd, timeout=10)
        return out.strip() if rc == 0 else ""

    cpu_model = ""
    cpu_count = os.cpu_count()
    try:
        with open("/proc/cpuinfo") as f:
            for line in f:
                if line.startswith("model name"):
                    cpu_model = line.split(":", 1)[1].strip()
                    break
    except OSError:
        pass

    fs_workspace = kv(["stat", "-f", "-c", "%T", str(Path(__file__).resolve().parent.parent)])
    fs_tmp = kv(["stat", "-f", "-c", "%T", "/tmp"])

    tools = {}
    for t in ("perf", "strace", "iostat", "pidstat", "mpstat", "sar"):
        tools[t] = bool(shutil.which(t))

    return {
        "platform": platform.platform(),
        "os": platform.system(),
        "kernel": platform.release(),
        "cpu_model": cpu_model,
        "cpu_count": cpu_count,
        "python": platform.python_version(),
        "workspace_fs": fs_workspace,
        "tmp_fs": fs_tmp,
        "profiling_tools": tools,
        "proc_vmhwm_available": os.path.exists("/proc/self/status"),
        "drop_caches_writable": os.access("/proc/sys/vm/drop_caches", os.W_OK),
        "timestamp": time.strftime("%Y-%m-%dT%H:%M:%S"),
    }


# --------------------------------------------------------------------------
# Statistics
# --------------------------------------------------------------------------

@dataclass
class Stat:
    """Summary of a numeric sample: median plus spread."""

    n: int
    median: float
    min: float
    max: float
    mean: float

    @staticmethod
    def of(samples: list[float]) -> "Stat":
        if not samples:
            return Stat(0, 0.0, 0.0, 0.0, 0.0)
        return Stat(
            n=len(samples),
            median=statistics.median(samples),
            min=min(samples),
            max=max(samples),
            mean=statistics.fmean(samples),
        )

    def as_dict(self) -> dict:
        return {
            "n": self.n,
            "median": self.median,
            "min": self.min,
            "max": self.max,
            "mean": self.mean,
        }


def percentile(samples: list[float], pct: float) -> float:
    """Nearest-rank percentile over a non-empty sample list."""
    if not samples:
        return 0.0
    s = sorted(samples)
    rank = int(-(-pct / 100.0 * len(s) // 1))  # ceil
    idx = min(max(rank - 1, 0), len(s) - 1)
    return s[idx]


# --------------------------------------------------------------------------
# SwiftFS metrics endpoint
# --------------------------------------------------------------------------

# Counters scraped from the daemon's /metrics endpoint. Each is reported as a
# delta around a measured phase. When the endpoint is disabled every value is
# ``None`` (unknown), never a fabricated zero.
METRIC_KEYS = (
    "swiftfs_chunk_requests_total",
    "swiftfs_ram_cache_hits_total",
    "swiftfs_disk_cache_hits_total",
    "swiftfs_backend_fetches_total",
    "swiftfs_backend_bytes_total",
    "swiftfs_coalesced_requests_total",
    "swiftfs_integrity_failures_total",
    "swiftfs_fetch_failures_total",
)


def parse_metrics(text: str) -> dict:
    """Parse a Prometheus-text body into {metric_name: float} (counters)."""
    out: dict = {}
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split()
        if len(parts) < 2:
            continue
        name = parts[0].split("{", 1)[0]
        if name in METRIC_KEYS:
            try:
                out[name] = float(parts[1])
            except ValueError:
                continue
    return out


def scrape_metrics(port: Optional[int], timeout: float = 3.0) -> Optional[dict]:
    """Fetch and parse /metrics; ``None`` if disabled or unreachable."""
    if port is None:
        return None
    import urllib.request

    try:
        with urllib.request.urlopen(
            f"http://127.0.0.1:{port}/metrics", timeout=timeout
        ) as r:
            return parse_metrics(r.read().decode())
    except Exception:
        return None


def await_metrics(port: int, timeout_s: float = 5.0) -> bool:
    """Wait until the metrics endpoint answers, so the pre-read scrape is real."""
    import urllib.request

    t0 = time.perf_counter()
    while time.perf_counter() - t0 < timeout_s:
        try:
            with urllib.request.urlopen(
                f"http://127.0.0.1:{port}/metrics", timeout=1
            ) as r:
                r.read()
            return True
        except Exception:
            time.sleep(0.01)
    return False


def counter_delta(before: Optional[dict], after: Optional[dict], key: str) -> Optional[float]:
    """Delta of ``key`` between two scrapes, or ``None`` if either is unknown."""
    if before is None or after is None:
        return None
    return after.get(key, 0.0) - before.get(key, 0.0)


class EnvRecorder:
    """Samples VmHWM of a process to capture peak RSS over its lifetime."""

    def __init__(self, pid: int):
        self.pid = pid
        self.peak_kb = 0
        self._tick()

    def _tick(self) -> None:
        kb = process_peak_rss_kb(self.pid)
        if kb is not None:
            self.peak_kb = max(self.peak_kb, kb)

    def stop(self) -> Optional[int]:
        self._tick()
        return self.peak_kb or None
