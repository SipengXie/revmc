"""Aggregate per-target-tx perf counters across r1/r2/r3 runs.

Each input is the JSON dump from skeleton_vs_perhash_bench --perf-out, which
records counters bracketed precisely around the JIT call for every target
bytecode_hash. The bracket is the same Instant::now() envelope used to compute
speedup_ratio in skeleton_vs_perhash.json — so this matches the 1.17x logical
work exactly.

Outputs:
  perf_per_target_summary.csv   — flat per-mode means + ratios
  perf_per_target_summary.json  — full nested structure (per-run, per-mode)
  perf_per_target_per_contract.csv — per-code_hash deltas (cycles, etc.)
"""
from __future__ import annotations

import csv
import os
import json
import statistics
from pathlib import Path

OUT = Path(os.environ.get("OUT_DIR") or Path(__file__).resolve().parent / "output")
OUT.mkdir(parents=True, exist_ok=True)
RUN_TAGS = ["r1", "r2", "r3"]
MODES = ["per_hash", "skeleton"]


def load_one(mode: str, tag: str) -> dict:
    p = OUT / f"perf_per_target_{mode}.{tag}.json"
    return json.loads(p.read_text())


def aggregate():
    """Returns {mode: {metric: [3 run values]}}."""
    table = {m: {} for m in MODES}
    for mode in MODES:
        for tag in RUN_TAGS:
            d = load_one(mode, tag)
            key = f"{mode}_mode"
            block = d[key]
            for metric in [
                "total_calls",
                "total_cycles",
                "total_instructions",
                "total_l1i_misses",
                "total_itlb_misses",
                "total_branches",
                "total_branch_misses",
            ]:
                table[mode].setdefault(metric, []).append(block[metric])
            ins = block["total_instructions"]
            br = block["total_branches"]
            cyc = block["total_cycles"]
            calls = block["total_calls"]
            table[mode].setdefault("CPI", []).append(cyc / ins if ins else 0)
            table[mode].setdefault("cycles_per_call", []).append(cyc / calls if calls else 0)
            table[mode].setdefault("instructions_per_call", []).append(
                ins / calls if calls else 0
            )
            table[mode].setdefault("L1i_miss_per_kinstr", []).append(
                block["total_l1i_misses"] / ins * 1000.0 if ins else 0
            )
            table[mode].setdefault("iTLB_miss_per_kinstr", []).append(
                block["total_itlb_misses"] / ins * 1000.0 if ins else 0
            )
            table[mode].setdefault("branch_miss_per_kbranch", []).append(
                block["total_branch_misses"] / br * 1000.0 if br else 0
            )
    return table


def per_contract_compare():
    """Per code_hash deltas: skelot/per_hash for cycles, instructions, etc.

    Sums counts across r1/r2/r3 within each mode, then computes per-contract
    cycles_skelot/cycles_per_hash etc.
    """
    by_hash = {}  # code_hash -> {mode -> sum dict}
    for mode in MODES:
        for tag in RUN_TAGS:
            d = load_one(mode, tag)
            for row in d[f"{mode}_mode"]["per_hash"]:
                e = by_hash.setdefault(row["code_hash"], {}).setdefault(
                    mode,
                    {
                        "calls": 0,
                        "cycles": 0,
                        "instructions": 0,
                        "l1i_misses": 0,
                        "itlb_misses": 0,
                        "branches": 0,
                        "branch_misses": 0,
                    },
                )
                for k in e:
                    e[k] += row[k]
    rows = []
    for h, modes in by_hash.items():
        if "per_hash" not in modes or "skeleton" not in modes:
            continue
        ph = modes["per_hash"]
        sk = modes["skeleton"]
        if ph["calls"] == 0 or sk["calls"] == 0:
            continue
        rows.append(
            {
                "code_hash": h,
                "ph_calls": ph["calls"],
                "sk_calls": sk["calls"],
                "ph_cycles_per_call": ph["cycles"] / ph["calls"],
                "sk_cycles_per_call": sk["cycles"] / sk["calls"],
                "cyc_per_call_ratio_skelot_over_ph": (
                    (sk["cycles"] / sk["calls"]) / (ph["cycles"] / ph["calls"])
                ),
                "ph_ipc": ph["instructions"] / ph["cycles"] if ph["cycles"] else 0,
                "sk_ipc": sk["instructions"] / sk["cycles"] if sk["cycles"] else 0,
                "ph_l1i_per_kinstr": ph["l1i_misses"] / ph["instructions"] * 1000.0
                if ph["instructions"]
                else 0,
                "sk_l1i_per_kinstr": sk["l1i_misses"] / sk["instructions"] * 1000.0
                if sk["instructions"]
                else 0,
                "ph_itlb_per_kinstr": ph["itlb_misses"] / ph["instructions"] * 1000.0
                if ph["instructions"]
                else 0,
                "sk_itlb_per_kinstr": sk["itlb_misses"] / sk["instructions"] * 1000.0
                if sk["instructions"]
                else 0,
                "ph_brmiss_per_kbr": ph["branch_misses"]
                / ph["branches"]
                * 1000.0
                if ph["branches"]
                else 0,
                "sk_brmiss_per_kbr": sk["branch_misses"]
                / sk["branches"]
                * 1000.0
                if sk["branches"]
                else 0,
            }
        )
    return rows


def main():
    table = aggregate()
    metrics = list(table["per_hash"].keys())
    summary = {"per_run": table, "ratios": {}}
    print(f"3-run aggregate, per-target-tx bracket\n")
    fmt = "{:<28}{:>16}{:>16}{:>10}{:>14}"
    print(fmt.format("metric", "per_hash mean", "skelot mean", "ratio", "ratio_range"))
    print("-" * 84)
    csv_rows = []
    for m in metrics:
        ph_vals = table["per_hash"][m]
        sk_vals = table["skeleton"][m]
        ph_mean = statistics.fmean(ph_vals)
        sk_mean = statistics.fmean(sk_vals)
        ratios = [s / p if p else float("nan") for p, s in zip(ph_vals, sk_vals)]
        rstr = f"[{min(ratios):.3f}, {max(ratios):.3f}]"
        ratio_mean = statistics.fmean(ratios)
        summary["ratios"][m] = {
            "ph_mean": ph_mean,
            "ph_stdev": statistics.stdev(ph_vals) if len(ph_vals) > 1 else 0.0,
            "sk_mean": sk_mean,
            "sk_stdev": statistics.stdev(sk_vals) if len(sk_vals) > 1 else 0.0,
            "ratio_mean": ratio_mean,
            "ratio_stdev": statistics.stdev(ratios) if len(ratios) > 1 else 0.0,
            "ratio_min": min(ratios),
            "ratio_max": max(ratios),
            "per_run_per_hash": ph_vals,
            "per_run_skelot": sk_vals,
        }
        print(fmt.format(m, f"{ph_mean:.4g}", f"{sk_mean:.4g}", f"{ratio_mean:.4f}", rstr))
        csv_rows.append(
            {
                "metric": m,
                "ph_mean": ph_mean,
                "sk_mean": sk_mean,
                "ratio_mean": ratio_mean,
                "ratio_min": min(ratios),
                "ratio_max": max(ratios),
                "n_runs": len(ratios),
            }
        )

    (OUT / "perf_per_target_summary.json").write_text(json.dumps(summary, indent=2))
    with open(OUT / "perf_per_target_summary.csv", "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=list(csv_rows[0].keys()))
        w.writeheader()
        w.writerows(csv_rows)

    # Per-contract comparison
    pc_rows = per_contract_compare()
    if pc_rows:
        with open(OUT / "perf_per_target_per_contract.csv", "w", newline="") as f:
            w = csv.DictWriter(f, fieldnames=list(pc_rows[0].keys()))
            w.writeheader()
            w.writerows(pc_rows)
        # Distribution of per-contract speedup
        import statistics as st

        ratios = [r["cyc_per_call_ratio_skelot_over_ph"] for r in pc_rows]
        ratios.sort()
        median = st.median(ratios)
        q25 = ratios[len(ratios) // 4] if len(ratios) >= 4 else ratios[0]
        q75 = ratios[3 * len(ratios) // 4] if len(ratios) >= 4 else ratios[-1]
        print(
            f"\nPer-target-contract cycle/call ratio (skelot/per_hash) over n={len(ratios)} contracts:"
        )
        print(f"  median = {median:.4f}")
        print(f"  IQR    = [{q25:.4f}, {q75:.4f}]")
        print(f"  range  = [{min(ratios):.4f}, {max(ratios):.4f}]")
        n_skel_faster = sum(1 for r in ratios if r < 1.0)
        n_per_hash_faster = sum(1 for r in ratios if r > 1.0)
        print(
            f"  skelot faster (ratio<1): {n_skel_faster}/{len(ratios)} = "
            f"{100*n_skel_faster/len(ratios):.1f}%"
        )
        print(
            f"  per_hash faster (ratio>1): {n_per_hash_faster}/{len(ratios)} = "
            f"{100*n_per_hash_faster/len(ratios):.1f}%"
        )


if __name__ == "__main__":
    main()
