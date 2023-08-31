#!/usr/bin/env python3
"""Parameter sweep: builds an .knut file with different options and reports size/time."""
import re, subprocess, sys, time, os

pbf = sys.argv[1]
out = sys.argv[2] if len(sys.argv) > 2 else "/tmp/knut-sweep.knut"
variants = [a.split() for a in sys.argv[3:]] or [
    ["--block-kib", "128"], ["--block-kib", "256"], ["--block-kib", "512"],
    ["--block-kib", "256", "--zstd-level", "19"], ["--block-kib", "256", "--no-dict"],
    ["--block-kib", "256", "--keyframe", "64"], ["--block-kib", "256", "--keyframe", "4"],
]
pbf_size = os.path.getsize(pbf)
print(f"{'bytes':>12} {'ratio':>6} {'secs':>6} {'rssMiB':>7}  args")
for args in variants:
    t = time.time()
    p = subprocess.run(["./target/release/knut", "build", pbf, "-o", out, *args], capture_output=True, text=True)
    dt = time.time() - t
    if p.returncode != 0:
        print("FAILED", args, p.stderr[-500:]); continue
    size = int(re.search(r"^output\s+(\d+)", p.stderr, re.M).group(1))
    rss = float(re.search(r"peak RSS\s+([\d.]+) MiB", p.stderr).group(1))
    print(f"{size:>12} {size/pbf_size:>6.3f} {dt:>6.1f} {rss:>7.0f}  {' '.join(args)}", flush=True)
os.remove(out)
