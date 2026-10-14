#!/usr/bin/env python3
"""Read-only listing benchmarks with disposable xses caches (not cold-disk tests)."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import statistics
import re
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser()
parser.add_argument("binary", nargs="?", default="target/release/xses")
parser.add_argument("--baseline")
parser.add_argument("--runs", type=int, default=10)
args = parser.parse_args()
if args.runs < 1:
    parser.error('--runs must be at least 1')
root = Path(tempfile.mkdtemp(prefix="xses-benchmark-"))
results = []
for label, binary in [("before", args.baseline), ("after", args.binary)]:
    if not binary:
        continue
    env = os.environ.copy()
    real_data = Path(env.get("XDG_DATA_HOME", str(Path.home() / ".local/share")))
    if "OPENCODE_DATA_DIR" not in env and "OPencode_DATA_DIR" not in env:
        env["OPENCODE_DATA_DIR"] = str(real_data / "opencode")
    env["XDG_DATA_HOME"] = str(root / label)
    env["XSES_NO_PROGRESS"] = "1"
    for case, argv, repeats in [
        ("help", ["--help"], args.runs),
        ("ls empty app cache", ["ls"], 1),
        ("ls warm", ["ls"], args.runs),
        ("ls pi warm", ["ls", "pi"], args.runs),
        ("ls all warm", ["ls", "--all"], args.runs),
        ("ls count warm", ["ls", "--count"], args.runs),
    ]:
        samples = []
        observations = []
        for _ in range(repeats):
            start = time.perf_counter()
            proc = subprocess.run([str(Path(binary).resolve()), *argv], stdout=subprocess.PIPE,
                                  stderr=subprocess.PIPE, env=env, timeout=60)
            if proc.returncode:
                raise RuntimeError(proc.stderr.decode())
            samples.append((time.perf_counter() - start) * 1000)
            output = proc.stdout.decode()
            total = re.search(r'^([0-9]+) sessions$', output, re.MULTILINE)
            observations.append(dict(stdout_sha256=hashlib.sha256(proc.stdout).hexdigest(),
                listed_sessions=int(total[1]) if total else None,
                store_counts=dict(re.findall(r'^(claude|codex|pi|omp|opencode)\s+([0-9]+) sessions$', output, re.MULTILINE))))
        result = dict(binary=label, case=case, median_ms=round(statistics.median(samples), 2),
                      min_ms=round(min(samples), 2), max_ms=round(max(samples), 2), samples_ms=samples,
                      observations=observations)
        results.append(result)
        print(label, case, result["median_ms"], "ms", flush=True)
(root / "results.json").write_text(json.dumps(results, indent=2))
print("Evidence:", root / "results.json")
