#!/usr/bin/env python3
"""Measure one local directory on the same SSD with interleaved thread counts.

Build the release CLI first. The report omits absolute scan paths and names of
individual files; only the dataset label and aggregate counters are emitted.
"""
import argparse
import json
import platform
import random
import statistics
import subprocess
import time
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("path", type=Path)
parser.add_argument("--label", default="local directory")
parser.add_argument("--runs", type=int, default=5)
parser.add_argument("--cli", type=Path, default=Path("target/release/tessera"))
parser.add_argument("--threads", type=int, nargs="+", default=[1, 2, 4, 8, 16])
parser.add_argument("--idle", type=int, default=10)
args = parser.parse_args()
if args.runs < 1 or any(n < 1 for n in args.threads):
    parser.error("runs and thread counts must be positive")

def run(*options):
    start = time.perf_counter()
    result = subprocess.run([str(args.cli.resolve()), *map(str, options)],
                            text=True, capture_output=True, check=True, timeout=180)
    record = json.loads(result.stdout)
    record.pop("path", None)
    record["wall_ms"] = round((time.perf_counter() - start) * 1000, 2)
    return record

rng = random.Random(0x5453524C)
rows = {n: [] for n in args.threads}
for _ in range(args.runs + 1):
    order = list(args.threads)
    rng.shuffle(order)
    for n in order:
        rows[n].append(run("scan", args.path, "--json", "--threads", n))
report = {
    "dataset": args.label,
    "machine": subprocess.check_output(["sysctl", "-n", "hw.model"], text=True).strip(),
    "arch": platform.machine(),
    "macos": platform.mac_ver()[0],
    "cache_policy": "one initial observation plus repeated warm runs; interleaved in deterministic random order; no cache purge",
    "rows": [{"threads": n, "first_run": records[0], "warm_runs": records[1:],
              "median_elapsed_ms": statistics.median(r["elapsed_ms"] for r in records[1:]),
              "median_wall_ms": statistics.median(r["wall_ms"] for r in records[1:])}
             for n, records in rows.items()],
}
if args.idle > 0:
    report["idle_with_fsevents"] = run("bench", args.path, "--runs", 1, "--idle", args.idle, "--watch")
print(json.dumps(report, indent=2))
