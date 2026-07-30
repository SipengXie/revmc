"""Aggregate the 3 wall_r* runs of skeleton_vs_perhash_bench --perf-out
that include per-call wall_ns. Compares in-bracket cycle ratio against
in-bracket wall_ns ratio for the same target contracts.

Key question: does the in-bracket wall ratio match the cycle ratio?
- If yes: 1.17×/1.25× gap is entirely OUTSIDE the bracket (= EVM
  framework around each call: frame loop, journaling, gas).
- If no: bracket itself contains mode-invariant overhead (likely the
  two perf_event_open read() syscalls per call, ~2-4 µs each), which
  dilutes the wall ratio toward 1.

Outputs:
  perf_p2_inbracket_summary.{csv,json}
  perf_p2_inbracket_per_contract.csv   per-target wall vs cyc ratio
"""
from __future__ import annotations

import csv
import os
import json
import statistics
from collections import defaultdict
from pathlib import Path

import numpy as np
from scipy import stats as sstats

OUT = Path(os.environ.get("OUT_DIR") or Path(__file__).resolve().parent / "output")
OUT.mkdir(parents=True, exist_ok=True)
RUN_TAGS = ["wall_r1", "wall_r2", "wall_r3"]
MODES = ["per_hash", "skeleton"]


def load(mode: str, tag: str) -> dict:
    p = OUT / f"perf_per_target_{mode}.{tag}.json"
    return json.loads(p.read_text())


def main():
    # totals per (mode, tag)
    totals = {m: {} for m in MODES}
    for mode in MODES:
        for tag in RUN_TAGS:
            d = load(mode, tag)
            block = d[f"{mode}_mode"]
            t = totals[mode].setdefault("totals", []).append(block)

    # Aggregate counts per code_hash across r1/r2/r3 within each mode
    by_hash = {}
    for mode in MODES:
        for tag in RUN_TAGS:
            d = load(mode, tag)
            for row in d[f"{mode}_mode"]["per_hash"]:
                e = by_hash.setdefault(row["code_hash"], {}).setdefault(
                    mode,
                    {"calls": 0, "cycles": 0, "wall_ns": 0, "instructions": 0},
                )
                e["calls"] += row["calls"]
                e["cycles"] += row["cycles"]
                e["wall_ns"] += row["wall_ns"]
                e["instructions"] += row["instructions"]

    # Per-contract ratios
    rows = []
    for h, modes in by_hash.items():
        if "per_hash" not in modes or "skeleton" not in modes:
            continue
        ph = modes["per_hash"]
        sk = modes["skeleton"]
        if ph["calls"] == 0 or sk["calls"] == 0:
            continue
        ph_cyc_per_call = ph["cycles"] / ph["calls"]
        sk_cyc_per_call = sk["cycles"] / sk["calls"]
        ph_wall_per_call = ph["wall_ns"] / ph["calls"]
        sk_wall_per_call = sk["wall_ns"] / sk["calls"]
        rows.append(
            {
                "code_hash": h,
                "ph_calls": ph["calls"],
                "sk_calls": sk["calls"],
                "ph_cyc_per_call": ph_cyc_per_call,
                "sk_cyc_per_call": sk_cyc_per_call,
                "ph_wall_ns_per_call": ph_wall_per_call,
                "sk_wall_ns_per_call": sk_wall_per_call,
                "cyc_ratio_in_bracket": sk_cyc_per_call / ph_cyc_per_call,
                "wall_ratio_in_bracket": sk_wall_per_call / ph_wall_per_call,
                "ph_inferred_GHz": ph_cyc_per_call / ph_wall_per_call,
                "sk_inferred_GHz": sk_cyc_per_call / sk_wall_per_call,
            }
        )

    with open(OUT / "perf_p2_inbracket_per_contract.csv", "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=list(rows[0].keys()))
        w.writeheader()
        w.writerows(rows)

    # Aggregate stats
    cyc_ratios = np.array([r["cyc_ratio_in_bracket"] for r in rows])
    wall_ratios = np.array([r["wall_ratio_in_bracket"] for r in rows])

    pearson_r, pearson_p = sstats.pearsonr(cyc_ratios, wall_ratios)
    spearman_r, spearman_p = sstats.spearmanr(cyc_ratios, wall_ratios)

    # Run-level totals (sum r1+r2+r3 within mode)
    sum_cyc = {m: sum(t["total_cycles"] for t in totals[m]["totals"]) for m in MODES}
    sum_wall = {m: sum(t["total_wall_ns"] for t in totals[m]["totals"]) for m in MODES}
    sum_calls = {m: sum(t["total_calls"] for t in totals[m]["totals"]) for m in MODES}
    sum_inst = {m: sum(t["total_instructions"] for t in totals[m]["totals"]) for m in MODES}

    summary = {
        "n_contracts": len(rows),
        "n_runs": len(RUN_TAGS),
        "totals_summed_over_runs": {
            "per_hash": {
                "calls": sum_calls["per_hash"],
                "cycles": sum_cyc["per_hash"],
                "wall_ns": sum_wall["per_hash"],
                "instructions": sum_inst["per_hash"],
                "per_call_cycles": sum_cyc["per_hash"] / sum_calls["per_hash"],
                "per_call_wall_ns": sum_wall["per_hash"] / sum_calls["per_hash"],
                "inferred_GHz": sum_cyc["per_hash"] / sum_wall["per_hash"],
            },
            "skeleton": {
                "calls": sum_calls["skeleton"],
                "cycles": sum_cyc["skeleton"],
                "wall_ns": sum_wall["skeleton"],
                "instructions": sum_inst["skeleton"],
                "per_call_cycles": sum_cyc["skeleton"] / sum_calls["skeleton"],
                "per_call_wall_ns": sum_wall["skeleton"] / sum_calls["skeleton"],
                "inferred_GHz": sum_cyc["skeleton"] / sum_wall["skeleton"],
            },
        },
        "global_ratios_skelot_over_per_hash": {
            "in_bracket_cycle_ratio": sum_cyc["skeleton"] / sum_cyc["per_hash"],
            "in_bracket_wall_ratio": sum_wall["skeleton"] / sum_wall["per_hash"],
        },
        "per_contract_distribution": {
            "median_cyc_ratio_in_bracket": float(np.median(cyc_ratios)),
            "median_wall_ratio_in_bracket": float(np.median(wall_ratios)),
            "mean_wall_minus_cyc_in_bracket": float(np.mean(wall_ratios - cyc_ratios)),
            "stdev_wall_minus_cyc_in_bracket": float(np.std(wall_ratios - cyc_ratios)),
            "pearson_in_bracket_wall_vs_cyc": {
                "r": float(pearson_r),
                "p": float(pearson_p),
            },
            "spearman_in_bracket_wall_vs_cyc": {
                "rho": float(spearman_r),
                "p": float(spearman_p),
            },
        },
    }

    # Implied per-call framework overhead inside bracket
    # If wall_per_call = jit_cycles / freq + bracket_overhead,
    # and assume freq stable, wall_ratio = (cyc_ratio * jit + ovh) / (jit + ovh)
    # Solve for ovh / jit = x. From median:
    median_cyc_ratio = summary["per_contract_distribution"]["median_cyc_ratio_in_bracket"]
    median_wall_ratio = summary["per_contract_distribution"]["median_wall_ratio_in_bracket"]
    # wall_ratio = (median_cyc_ratio + x) / (1 + x) where x = ovh / jit_per_call
    # x = (median_cyc_ratio - median_wall_ratio) / (median_wall_ratio - 1)
    if median_wall_ratio != 1.0:
        x = (median_cyc_ratio - median_wall_ratio) / (median_wall_ratio - 1.0)
        bracket_overhead_share = x / (1.0 + x) if x is not None and (1 + x) != 0 else None
        summary["bracket_overhead_model"] = {
            "x_overhead_to_jit_ratio": x,
            "bracket_overhead_share_of_per_call_wall_pct": bracket_overhead_share * 100.0
            if bracket_overhead_share is not None
            else None,
        }

    (OUT / "perf_p2_inbracket_summary.json").write_text(json.dumps(summary, indent=2))

    # Pretty print
    print(f"P2 in-bracket — n_contracts = {len(rows)}, n_runs = {len(RUN_TAGS)}")
    print()
    print("Global totals (summed r1+r2+r3 within mode):")
    g = summary["totals_summed_over_runs"]
    for m in ["per_hash", "skeleton"]:
        print(
            f"  {m}: calls={g[m]['calls']}  cyc/call={g[m]['per_call_cycles']:.0f}  "
            f"wall_ns/call={g[m]['per_call_wall_ns']:.0f}  inferred GHz={g[m]['inferred_GHz']:.3f}"
        )
    gr = summary["global_ratios_skelot_over_per_hash"]
    print(
        f"  global cycle ratio (sk/ph) = {gr['in_bracket_cycle_ratio']:.4f}  "
        f"(speedup {1/gr['in_bracket_cycle_ratio']:.4f}x)"
    )
    print(
        f"  global wall ratio  (sk/ph) = {gr['in_bracket_wall_ratio']:.4f}  "
        f"(speedup {1/gr['in_bracket_wall_ratio']:.4f}x)"
    )
    print()
    print("Per-contract distribution:")
    pc = summary["per_contract_distribution"]
    print(f"  median cyc ratio (sk/ph): {pc['median_cyc_ratio_in_bracket']:.4f}")
    print(f"  median wall ratio (sk/ph): {pc['median_wall_ratio_in_bracket']:.4f}")
    print(
        f"  Pearson  r(wall_ratio, cyc_ratio) = {pc['pearson_in_bracket_wall_vs_cyc']['r']:.4f} "
        f"(p={pc['pearson_in_bracket_wall_vs_cyc']['p']:.2e})"
    )
    print(
        f"  Spearman ρ(wall_ratio, cyc_ratio) = {pc['spearman_in_bracket_wall_vs_cyc']['rho']:.4f} "
        f"(p={pc['spearman_in_bracket_wall_vs_cyc']['p']:.2e})"
    )
    if "bracket_overhead_model" in summary:
        b = summary["bracket_overhead_model"]
        print()
        print("Bracket-internal overhead model:")
        print(f"  x = bracket_overhead / jit_cycles_per_call_in_us = {b['x_overhead_to_jit_ratio']:.3f}")
        print(
            f"  bracket overhead share of per-call wall = {b['bracket_overhead_share_of_per_call_wall_pct']:.1f}%"
        )


if __name__ == "__main__":
    main()
