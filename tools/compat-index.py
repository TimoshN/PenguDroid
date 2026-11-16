#!/usr/bin/env python3
"""Generate an explicitly scoped compatibility inventory and README SVG.

Input comes from `pengudroid compat-index` and runtime diagnostics. A wired
JNI function is not necessarily conformant; no global Android percentage is
calculated. Trace observations are a lower bound, not an exhaustive API audit.
"""
import argparse
from collections import Counter
import hashlib
import html
import json
from pathlib import Path
import re
import statistics

ROOT = Path(__file__).resolve().parent.parent

def observations(paths):
    methods, slots = set(), set()
    for path in paths:
        text = path.read_text(errors="replace")
        methods.update(re.findall(r"\[pengu-vm\] UNIMPLEMENTED method ([^\r\n]+)", text))
        slots.update(map(int, re.findall(r"UNIMPLEMENTED JNI slot (\d+)", text)))
    return sorted(methods), sorted(slots)

def benchmark(path):
    if not path:
        return None
    values = [float(v) for v in re.findall(r"ns_per_call=([0-9.]+)", path.read_text())]
    if not values:
        raise ValueError(f"no dispatch samples in {path}")
    return {"samples": values, "median_ns_per_call": statistics.median(values)}

def inventory(snapshot, catalog, logs, before=None, after=None):
    total = snapshot["jni_function_slots"]
    wired = sorted(set(snapshot["wired_jni_slots"]))
    if total != 229 or any(not 4 <= slot <= 232 for slot in wired):
        raise ValueError("unexpected JNI 1.6 table shape")
    if snapshot.get("forbidden_android_runtime_mappings"):
        raise ValueError("inventory process loaded ART/framework components")
    methods, slots = observations(logs)
    counts = Counter(row["status"] for row in catalog["scenarios"])
    if set(counts) - {"passed", "partial", "missing"}:
        raise ValueError("unknown scenario status")
    sources = list((ROOT / "crates/pengudroid-dexvm/src").glob("*"))
    return {"schema": 1, "version": snapshot["version"], "backend": "pengu",
        "scope": catalog["scope"], "whole_android_compatibility_percent": None,
        "jni": {"function_slots": total, "reserved_slots_excluded": 4,
            "wired_slots": wired, "placeholder_slots": sorted(set(range(4, 233)) - set(wired)),
            "note": "Specific function handlers include partial/no-op implementations; wiring is not semantic conformance."},
        "scenarios": catalog["scenarios"], "scenario_counts": dict(sorted(counts.items())),
        "observations": {"exhaustive": False, "unimplemented_methods": methods,
            "unimplemented_jni_slots": slots, "trace_files": [p.name for p in logs]},
        "dispatch_microbenchmark": {"before": benchmark(before), "after": benchmark(after),
            "note": "100000 calls, 16 DEX units, 1024 methods/class; focused lookup cost, not game FPS."},
        "source_sha256": {str(p.relative_to(ROOT)): hashlib.sha256(p.read_bytes()).hexdigest()
            for p in sorted(sources) if p.is_file()}}

def svg(data):
    wired, total = len(data["jni"]["wired_slots"]), data["jni"]["function_slots"]
    counts = data["scenario_counts"]
    esc = html.escape
    parts = ['<svg xmlns="http://www.w3.org/2000/svg" width="1000" height="430" viewBox="0 0 1000 430" role="img" aria-labelledby="title desc">',
        '<title id="title">PenguDroid independent runtime compatibility audit</title>',
        '<desc id="desc">JNI handler wiring and a scoped foundation scenario checklist. No overall Android compatibility percentage is asserted.</desc>',
        '<rect width="1000" height="430" rx="16" fill="#111827"/>',
        '<g font-family="sans-serif" fill="#e5e7eb">',
        '<text x="32" y="42" font-size="24" font-weight="bold">Independent DexVM · compatibility audit</text>',
        '<text x="32" y="72" font-size="15" fill="#a5b4fc">ART / framework payload excluded from the Pengu profile; legacy ART remains optional.</text>',
        f'<text x="32" y="115" font-size="17">JNI 1.6 function table: {wired} wired / {total - wired} placeholders / {total} function slots</text>',
        '<rect x="32" y="132" width="936" height="26" rx="6" fill="#374151"/>',
        f'<rect x="32" y="132" width="{936 * wired / total:.2f}" height="26" rx="6" fill="#38bdf8"/>',
        '<text x="32" y="183" font-size="14" fill="#9ca3af">Wired handlers can be partial or no-ops. This bar measures wiring, not Android conformance.</text>',
        f'<text x="32" y="225" font-size="17">Audited foundation scenarios · fixed scope: {len(data["scenarios"])} checks</text>']
    x = 32
    for label, color in [("passed", "#34d399"), ("partial", "#fbbf24"), ("missing", "#fb7185")]:
        n = counts.get(label, 0)
        width = 936 * n / len(data["scenarios"])
        parts += [f'<rect x="{x:.2f}" y="243" width="{width:.2f}" height="30" fill="{color}"/>']
        x += width
    parts += [f'<text x="32" y="300" font-size="16">Passed subset: {counts.get("passed",0)} · Partial: {counts.get("partial",0)} · Missing: {counts.get("missing",0)}</text>',
        '<text x="32" y="329" font-size="14" fill="#9ca3af">A passed scenario covers only its named regression test, not its entire API family.</text>',
        f'<text x="32" y="363" font-size="16">Observed missing calls: {len(data["observations"]["unimplemented_methods"])} Java signatures · {len(data["observations"]["unimplemented_jni_slots"])} JNI slots</text>',
        '<text x="32" y="394" font-size="14" fill="#fda4af">ZZZ independent gameplay: unverified / blocked. No whole-Android completion percentage.</text>', '</g></svg>']
    return "\n".join(parts) + "\n"

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inventory-json", type=Path, required=True)
    parser.add_argument("--catalog", type=Path, default=ROOT / "docs/compatibility/catalog.json")
    parser.add_argument("--trace-log", type=Path, action="append", default=[])
    parser.add_argument("--benchmark-before", type=Path)
    parser.add_argument("--benchmark-after", type=Path)
    parser.add_argument("--output", type=Path, default=ROOT / "docs/compatibility")
    args = parser.parse_args()
    data = inventory(json.loads(args.inventory_json.read_text()), json.loads(args.catalog.read_text()),
        args.trace_log, args.benchmark_before, args.benchmark_after)
    args.output.mkdir(parents=True, exist_ok=True)
    (args.output / "index.json").write_text(json.dumps(data, indent=2) + "\n")
    (args.output / "overview.svg").write_text(svg(data))
    print(f"Generated {args.output}: {len(data['jni']['wired_slots'])}/{data['jni']['function_slots']} JNI slots wired; {len(data['observations']['unimplemented_methods'])} observed missing signatures")

if __name__ == "__main__":
    main()
