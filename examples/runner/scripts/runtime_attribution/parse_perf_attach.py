"""Aggregate attach1/attach2/attach3 perf runs into per-mode mean +/- stddev.

The .stat files were produced by perf_b1_attach.sh, which attaches perf
via -p $PID immediately after the bench prints MEASUREMENT_PHASE_BEGIN
and stays attached until the bench exits. Each .stat covers exactly the
Phase 5 measurement loop for one --only-mode setting.

Output:
  perf_attach_summary.json  — per-event per-mode mean/stddev across 3 runs
  perf_attach_summary.csv   — flat per-event row with skelot/per_hash ratios
"""
from __future__ import annotations

import csv
import os
import json
from pathlib import Path
import statistics

OUT = Path(os.environ.get("OUT_DIR") or Path(__file__).resolve().parent / "output")
OUT.mkdir(parents=True, exist_ok=True)
RUN_TAGS = ["attach1", "attach2", "attach3"]
EVENT_SETS = ["icache", "other"]
MODES = ["per_hash", "skeleton"]


def parse_stat(path: Path) -> dict[str, float]:
    counts: dict[str, float] = {}
    if not path.exists():
        return counts
    for raw in path.read_text().splitlines():
        if not raw or raw.startswith("#"):
            continue
        parts = raw.split(",")
        if len(parts) < 3:
            continue
        try:
            count = float(parts[0])
        except ValueError:
            continue
        event = parts[2]
        counts[event] = count
        # Running time in ns (column 4)
        if len(parts) > 3:
            try:
                counts[f"{event}__running_ns"] = float(parts[3])
            except ValueError:
                pass
    return counts


def gather():
    """{tag: {mode: {event: count}}}"""
    runs = {}
    for tag in RUN_TAGS:
        runs[tag] = {}
        for mode in MODES:
            merged = {}
            for es in EVENT_SETS:
                p = OUT / f"perf_{mode}_{es}.{tag}.stat"
                merged.update(parse_stat(p))
            runs[tag][mode] = merged
    return runs


def derive(d: dict[str, float]) -> dict[str, float]:
    out = {}
    cyc = d.get("cycles", 0.0)
    ins = d.get("instructions", 0.0)
    if cyc and ins:
        out["CPI"] = cyc / ins
        out["L1i_miss_per_kinstr"] = (d.get("L1-icache-load-misses", 0.0) / ins) * 1000.0
        out["iTLB_miss_per_kinstr"] = (d.get("iTLB-load-misses", 0.0) / ins) * 1000.0
    if d.get("branches"):
        out["branch_miss_per_kbranch"] = (
            d.get("branch-misses", 0.0) / d["branches"]
        ) * 1000.0
    return out


def main():
    runs = gather()

    # Per-event per-mode list of values across runs
    raw_events = [
        "cycles",
        "instructions",
        "L1-icache-load-misses",
        "iTLB-load-misses",
        "branch-misses",
        "branches",
    ]
    derived_keys = ["CPI", "L1i_miss_per_kinstr", "iTLB_miss_per_kinstr", "branch_miss_per_kbranch"]

    summary = {"raw": {}, "derived": {}, "ratios": {}}
    for ev in raw_events:
        summary["raw"][ev] = {}
        for mode in MODES:
            vals = [runs[t][mode].get(ev) for t in RUN_TAGS if runs[t][mode].get(ev) is not None]
            if vals:
                summary["raw"][ev][mode] = {
                    "n": len(vals),
                    "mean": statistics.fmean(vals),
                    "stdev": statistics.stdev(vals) if len(vals) > 1 else 0.0,
                    "values": vals,
                }
    for k in derived_keys:
        summary["derived"][k] = {}
        for mode in MODES:
            vals = []
            for t in RUN_TAGS:
                d = derive(runs[t][mode])
                if k in d:
                    vals.append(d[k])
            if vals:
                summary["derived"][k][mode] = {
                    "n": len(vals),
                    "mean": statistics.fmean(vals),
                    "stdev": statistics.stdev(vals) if len(vals) > 1 else 0.0,
                    "values": vals,
                }

    # Skelot/per_hash ratios using means
    for k in derived_keys + raw_events:
        bucket = summary["derived"] if k in derived_keys else summary["raw"]
        if k in bucket and "per_hash" in bucket[k] and "skeleton" in bucket[k]:
            ph = bucket[k]["per_hash"]["mean"]
            sk = bucket[k]["skeleton"]["mean"]
            summary["ratios"][k] = {
                "skelot_over_per_hash": sk / ph if ph else None,
                "delta_pct": (sk - ph) / ph * 100.0 if ph else None,
            }

    # Per-run skelot/per_hash ratio (for stability of the ratio itself)
    summary["per_run_ratios"] = {}
    for k in derived_keys:
        per_run_vals = []
        for t in RUN_TAGS:
            ph_d = derive(runs[t]["per_hash"])
            sk_d = derive(runs[t]["skeleton"])
            if k in ph_d and k in sk_d and ph_d[k]:
                per_run_vals.append(sk_d[k] / ph_d[k])
        if per_run_vals:
            summary["per_run_ratios"][k] = {
                "n": len(per_run_vals),
                "mean": statistics.fmean(per_run_vals),
                "stdev": statistics.stdev(per_run_vals) if len(per_run_vals) > 1 else 0.0,
                "min": min(per_run_vals),
                "max": max(per_run_vals),
                "values": per_run_vals,
            }

    (OUT / "perf_attach_summary.json").write_text(json.dumps(summary, indent=2))

    # CSV: one row per metric
    with open(OUT / "perf_attach_summary.csv", "w", newline="") as f:
        w = csv.writer(f)
        w.writerow([
            "metric",
            "per_hash_mean",
            "per_hash_stdev",
            "skelot_mean",
            "skelot_stdev",
            "ratio_mean_skelot_over_ph",
            "ratio_min",
            "ratio_max",
            "n_runs",
        ])
        for k in derived_keys + raw_events:
            bucket = summary["derived"] if k in derived_keys else summary["raw"]
            if k not in bucket or "per_hash" not in bucket[k] or "skeleton" not in bucket[k]:
                continue
            ph = bucket[k]["per_hash"]
            sk = bucket[k]["skeleton"]
            pr = summary["per_run_ratios"].get(k, {})
            w.writerow(
                [
                    k,
                    f"{ph['mean']:.4g}",
                    f"{ph['stdev']:.4g}",
                    f"{sk['mean']:.4g}",
                    f"{sk['stdev']:.4g}",
                    f"{pr.get('mean', float('nan')):.4f}" if pr else "",
                    f"{pr.get('min', float('nan')):.4f}" if pr else "",
                    f"{pr.get('max', float('nan')):.4f}" if pr else "",
                    len(RUN_TAGS),
                ]
            )

    # Stdout
    print(f"3-run aggregate (per_hash mean ± stdev / skelot mean ± stdev / mean ratio)\n")
    fmt = "{:<28}{:>16}{:>16}{:>10}{:>14}"
    print(fmt.format("metric", "per_hash mean", "skelot mean", "ratio", "ratio_range"))
    print("-" * 84)
    for k in derived_keys + raw_events:
        bucket = summary["derived"] if k in derived_keys else summary["raw"]
        if k not in bucket or "per_hash" not in bucket[k] or "skeleton" not in bucket[k]:
            continue
        ph = bucket[k]["per_hash"]
        sk = bucket[k]["skeleton"]
        pr = summary["per_run_ratios"].get(k, {})
        ratio_str = f"{pr.get('mean', float('nan')):.4f}" if pr else f"{sk['mean']/ph['mean']:.4f}"
        rng = f"[{pr.get('min', 0):.3f}, {pr.get('max', 0):.3f}]" if pr else ""
        print(fmt.format(k, f"{ph['mean']:.3e}", f"{sk['mean']:.3e}", ratio_str, rng))


if __name__ == "__main__":
    main()
