"""P3 cross-instance B (data-table cost) measurement.

Reads perf_p3_{auto,forced}.r{1,2,3}.json (cycle bracket on the 85-family
subset under classify_mode=auto vs force-all-variant). For each target
contract whose family is covered by both caches, computes the per-call
cycle ratio between the two skelot variants:
    forced_cyc_per_call / auto_cyc_per_call

If the median ratio is close to 1, the data-table cost B is small in
the cross-instance regime — exactly the closure the reviewer asked for.

Outputs:
    perf_p3_per_contract.csv
    perf_p3_summary.json
"""
from __future__ import annotations

import csv
import os
import json
from pathlib import Path

import numpy as np
from scipy import stats as sstats

OUT = Path(os.environ.get("OUT_DIR") or Path(__file__).resolve().parent / "output")
OUT.mkdir(parents=True, exist_ok=True)
TAGS = ["r1", "r2", "r3"]
VARIANTS = ["auto", "forced"]


def load(variant: str, tag: str) -> dict:
    return json.loads((OUT / f"perf_p3_{variant}.{tag}.json").read_text())


def main():
    by_hash: dict[str, dict[str, dict[str, int]]] = {}
    for tag in TAGS:
        for v in VARIANTS:
            d = load(v, tag)
            for row in d["skeleton_mode"]["per_hash"]:
                e = by_hash.setdefault(row["code_hash"], {}).setdefault(
                    v,
                    {"calls": 0, "cycles": 0, "instructions": 0, "wall_ns": 0},
                )
                for k in e:
                    e[k] += row.get(k, 0)

    rows = []
    for h, modes in by_hash.items():
        if "auto" not in modes or "forced" not in modes:
            continue
        a = modes["auto"]
        f = modes["forced"]
        if a["calls"] == 0 or f["calls"] == 0:
            continue
        a_cyc_per_call = a["cycles"] / a["calls"]
        f_cyc_per_call = f["cycles"] / f["calls"]
        rows.append(
            {
                "code_hash": h,
                "auto_calls": a["calls"],
                "forced_calls": f["calls"],
                "auto_cyc_per_call": a_cyc_per_call,
                "forced_cyc_per_call": f_cyc_per_call,
                "ratio_forced_over_auto": f_cyc_per_call / a_cyc_per_call,
                "auto_wall_ns_per_call": a["wall_ns"] / a["calls"],
                "forced_wall_ns_per_call": f["wall_ns"] / f["calls"],
                "wall_ratio_forced_over_auto": (f["wall_ns"] / f["calls"])
                / (a["wall_ns"] / a["calls"]),
            }
        )

    if not rows:
        print("ERROR: no contracts shared between auto and forced")
        return

    with open(OUT / "perf_p3_per_contract.csv", "w", newline="") as fh:
        w = csv.DictWriter(fh, fieldnames=list(rows[0].keys()))
        w.writeheader()
        w.writerows(rows)

    cyc_ratios = np.array([r["ratio_forced_over_auto"] for r in rows])
    wall_ratios = np.array([r["wall_ratio_forced_over_auto"] for r in rows])

    # Global totals
    a_tot = {
        k: sum(modes["auto"][k] for modes in by_hash.values() if "auto" in modes)
        for k in ["calls", "cycles", "wall_ns"]
    }
    f_tot = {
        k: sum(modes["forced"][k] for modes in by_hash.values() if "forced" in modes)
        for k in ["calls", "cycles", "wall_ns"]
    }

    summary = {
        "n_contracts": len(rows),
        "n_runs": len(TAGS),
        "global_totals": {
            "auto": a_tot,
            "forced": f_tot,
            "global_cyc_ratio_forced_over_auto": (f_tot["cycles"] / f_tot["calls"])
            / (a_tot["cycles"] / a_tot["calls"]),
            "global_wall_ratio_forced_over_auto": (f_tot["wall_ns"] / f_tot["calls"])
            / (a_tot["wall_ns"] / a_tot["calls"]),
        },
        "per_contract_distribution": {
            "median_cyc_ratio_forced_over_auto": float(np.median(cyc_ratios)),
            "p25_cyc_ratio": float(np.percentile(cyc_ratios, 25)),
            "p75_cyc_ratio": float(np.percentile(cyc_ratios, 75)),
            "min_cyc_ratio": float(cyc_ratios.min()),
            "max_cyc_ratio": float(cyc_ratios.max()),
            "n_forced_faster_than_auto": int((cyc_ratios < 1.0).sum()),
            "n_auto_faster_than_forced": int((cyc_ratios > 1.0).sum()),
            "median_wall_ratio_forced_over_auto": float(np.median(wall_ratios)),
        },
        "interpretation": (
            "ratio = forced_cyc_per_call / auto_cyc_per_call. ratio > 1 means "
            "forced (every PUSH via data table) is slower than auto (most "
            "PUSHes inlined). |ratio - 1| upper-bounds the cross-instance "
            "data-table cost B."
        ),
    }

    (OUT / "perf_p3_summary.json").write_text(json.dumps(summary, indent=2))

    print(f"P3 — n_contracts = {len(rows)}, n_runs = {len(TAGS)}")
    g = summary["global_totals"]
    print(
        f"  auto totals:   calls={a_tot['calls']}  cyc/call={a_tot['cycles']/a_tot['calls']:.0f}"
    )
    print(
        f"  forced totals: calls={f_tot['calls']}  cyc/call={f_tot['cycles']/f_tot['calls']:.0f}"
    )
    print(f"  global cycle ratio (forced/auto): {g['global_cyc_ratio_forced_over_auto']:.4f}")
    print(f"  global wall  ratio (forced/auto): {g['global_wall_ratio_forced_over_auto']:.4f}")
    print()
    pc = summary["per_contract_distribution"]
    print("Per-contract distribution:")
    print(
        f"  median cycle ratio (forced/auto): {pc['median_cyc_ratio_forced_over_auto']:.4f}  "
        f"IQR=[{pc['p25_cyc_ratio']:.4f}, {pc['p75_cyc_ratio']:.4f}]  "
        f"range=[{pc['min_cyc_ratio']:.4f}, {pc['max_cyc_ratio']:.4f}]"
    )
    print(
        f"  forced faster: {pc['n_forced_faster_than_auto']}/{len(rows)};  "
        f"auto faster: {pc['n_auto_faster_than_forced']}/{len(rows)}"
    )
    print(f"  median wall ratio (forced/auto): {pc['median_wall_ratio_forced_over_auto']:.4f}")


if __name__ == "__main__":
    main()
