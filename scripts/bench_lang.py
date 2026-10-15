#!/usr/bin/env python3
"""Table of the knut language benchmarks (crates/knut-exec/benches/constructs.rs).

    python3 scripts/bench_lang.py                 # latest run ("new")
    python3 scripts/bench_lang.py initial         # a saved baseline
    python3 scripts/bench_lang.py initial new     # compare two runs: ratio new / initial

Times are medians. "net" subtracts the `overhead` benchmark (a script that reads no data),
so that the constructs can be compared with each other.
"""
import json, os, sys

root = os.path.join(os.path.dirname(__file__), "..", "target", "criterion", "knut-lang")


def load(baseline):
    out = {}
    if not os.path.isdir(root):
        sys.exit(f"no results in {root}: run `cargo bench -p knut-exec --bench constructs` first")
    for d in sorted(os.listdir(root)):
        est = os.path.join(root, d, baseline, "estimates.json")
        meta = os.path.join(root, d, baseline, "benchmark.json")
        if not os.path.exists(est):
            continue
        with open(meta) as f:
            name = json.load(f).get("value_str") or d
        with open(est) as f:
            out[name] = json.load(f)["median"]["point_estimate"] / 1e9
    return out


def fmt(s):
    if s is None:
        return "–"
    if s < 1e-3:
        return f"{s * 1e6:8.1f} µs"
    if s < 1:
        return f"{s * 1e3:8.2f} ms"
    return f"{s:8.2f} s "


a = load(sys.argv[1] if len(sys.argv) > 1 else "new")
b = load(sys.argv[2]) if len(sys.argv) > 2 else None
base = a.get("overhead", 0.0)
w = max(len(n) for n in a) if a else 10
if b is None:
    print(f"{'construct':<{w}}  {'median':>11}  {'net':>11}")
    for n, t in a.items():
        print(f"{n:<{w}}  {fmt(t)}  {fmt(max(t - base, 0.0)) if n != 'overhead' else '':>11}")
else:
    print(f"{'construct':<{w}}  {sys.argv[1]:>11}  {sys.argv[2]:>11}  {'ratio':>6}")
    for n in sorted(set(a) | set(b), key=lambda n: list(a).index(n) if n in a else len(a)):
        x, y = a.get(n), b.get(n)
        r = f"{y / x:6.2f}" if x and y else "–"
        flag = "  slower" if x and y and y / x > 1.1 else ("  faster" if x and y and y / x < 0.9 else "")
        print(f"{n:<{w}}  {fmt(x)}  {fmt(y)}  {r}{flag}")
