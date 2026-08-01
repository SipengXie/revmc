"""Response to four-part reviewer critique using only existing data files.

Addresses:
- Problem 2: cycle vs wall ratio reconciliation, per-contract.
- Problem 3: A (locality) vs B (data-table PUSH) decomposition via the
  existing all_variant ablation.
- Problem 4: characterization of the 49 SKELETON_SLOWER target contracts.

Outputs:
  critique_response_p2.csv     per-contract cycle ratio vs wall ratio + Pearson
  critique_response_p3.json    locality-vs-datatable decomposition table
  critique_response_p4.csv     slower-49 contracts joined with bucket / family_size / variants
  critique_response.md         human-readable summary of all four findings

This script reads only:
  joined_per_contract.csv                 (Stage A output)
  perf_per_target_per_contract.csv        (Stage B per-target output)
  ../data/ablation_allvariant.families.jsonl
  ../data/skeleton_mapping.tsv
"""
from __future__ import annotations

import csv
import json
import os
import statistics
from collections import defaultdict
from pathlib import Path

import numpy as np
from scipy import stats as sstats


def _env_path(name: str, what: str) -> Path:
    v = os.environ.get(name)
    if not v:
        raise SystemExit(
            f"Set {name} environment variable ({what}). See README.md in this directory."
        )
    return Path(v)


OUT = Path(os.environ.get("OUT_DIR") or Path(__file__).resolve().parent / "output")
OUT.mkdir(parents=True, exist_ok=True)
DATA = _env_path(
    "INPUT_DATA_DIR",
    "directory holding ablation_allvariant.families.jsonl etc.",
)


def load_joined() -> dict[str, dict]:
    rows = {}
    with open(OUT / "joined_per_contract.csv") as f:
        for r in csv.DictReader(f):
            rows[r["code_hash"].lower()] = r
    return rows


def load_perf_per_contract() -> dict[str, dict]:
    rows = {}
    with open(OUT / "perf_per_target_per_contract.csv") as f:
        for r in csv.DictReader(f):
            rows[r["code_hash"].lower()] = r
    return rows


def load_av_families() -> dict[str, dict]:
    fams = {}
    with open(DATA / "ablation_allvariant.families.jsonl") as f:
        for line in f:
            r = json.loads(line)
            fams[r["skeleton_id"]] = r
    return fams


def problem2_cycle_vs_wall():
    """Join cycle/call (perf) with wall us/call (skeleton_vs_perhash.json)
    per code_hash; quantify per-contract correlation AND fit a structural
    model that recovers the mode-invariant framework overhead.

    Model: wall_us = jit_cycles / freq + framework_overhead
    Then for the same workload across modes (same framework overhead):
      wall_ratio = (cyc_ratio + x) / (1 + x)
      where x = framework_overhead / (jit_cycles_per_call / freq)

    Linear regression of wall_ratio on cyc_ratio recovers slope = 1/(1+x),
    intercept = x/(1+x), so x = 1/slope - 1. The framework's share of
    per-call wall is then x / (1+x) = 1 - slope.
    """
    joined = load_joined()
    perf = load_perf_per_contract()

    rows = []
    for ch, p in perf.items():
        j = joined.get(ch)
        if j is None:
            continue
        wall_ratio = float(j["mean_skeleton_us"]) / float(j["mean_per_hash_us"])
        cyc_ratio = float(p["cyc_per_call_ratio_skelot_over_ph"])
        rows.append(
            {
                "code_hash": ch,
                "wall_skelot_over_ph": wall_ratio,
                "cyc_skelot_over_ph": cyc_ratio,
                "wall_minus_cyc": wall_ratio - cyc_ratio,
                "ph_calls": int(p["ph_calls"]),
                "sk_calls": int(p["sk_calls"]),
                "ph_wall_us_per_call": float(j["mean_per_hash_us"]),
                "sk_wall_us_per_call": float(j["mean_skeleton_us"]),
                "ph_cycles_per_call": float(p["ph_cycles_per_call"]),
                "sk_cycles_per_call": float(p["sk_cycles_per_call"]),
                "family_size": int(j["family_size"]),
                "bucket": j["bucket"],
                "speedup_ratio_orig": float(j["speedup_ratio"]),
            }
        )

    with open(OUT / "critique_response_p2.csv", "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=list(rows[0].keys()))
        w.writeheader()
        w.writerows(rows)

    cyc = np.array([r["cyc_skelot_over_ph"] for r in rows])
    wall = np.array([r["wall_skelot_over_ph"] for r in rows])
    pearson_r, pearson_p = sstats.pearsonr(cyc, wall)
    spearman_r, spearman_p = sstats.spearmanr(cyc, wall)

    median_cyc = float(np.median(cyc))
    median_wall = float(np.median(wall))

    # Linear regression: wall_ratio ~ a + b * cyc_ratio
    slope, intercept, r_value, p_value, _ = sstats.linregress(cyc, wall)
    # Solve for framework fraction
    x_ratio = 1.0 / slope - 1.0
    framework_share_of_wall = x_ratio / (1.0 + x_ratio)

    return {
        "n": len(rows),
        "median_cyc_ratio": median_cyc,
        "median_wall_ratio": median_wall,
        "implied_cyc_speedup": 1.0 / median_cyc,
        "implied_wall_speedup": 1.0 / median_wall,
        "pearson_cyc_vs_wall": {"r": float(pearson_r), "p": float(pearson_p)},
        "spearman_cyc_vs_wall": {"rho": float(spearman_r), "p": float(spearman_p)},
        "mean_wall_minus_cyc": float(np.mean(wall - cyc)),
        "stdev_wall_minus_cyc": float(np.std(wall - cyc)),
        "regression_wall_on_cyc": {
            "slope": float(slope),
            "intercept": float(intercept),
            "r_squared": float(r_value ** 2),
            "p_value": float(p_value),
            "framework_overhead_ratio_x": float(x_ratio),
            "framework_share_of_per_call_wall_pct": float(framework_share_of_wall * 100),
        },
    }


def problem3_locality_vs_datatable():
    """Three-way comparison per family using the existing all_variant ablation:

    - per_hash:    one .so per code_hash (no shared .text)
    - skelot:      one .so per family, variant PUSH via data-table load (A+B)
    - all_variant: one .so per family, ALL PUSH inlined as movq-imm (A only)

    If all_variant is faster than skelot, the difference quantifies the
    data-table cost B; if all_variant is similar to or slower than per_hash,
    the locality benefit A is small.
    """
    fams = load_av_families()

    rows = []
    for sid, r in fams.items():
        ph_ns = r["per_hash"]["exec_median_ns"]
        sk_ns = r["skelot"]["exec_median_ns"]
        av_ns = r["all_variant"]["exec_median_ns"]
        if not (ph_ns and sk_ns and av_ns):
            continue
        # Speedup expressed as per_hash / mode → >1 means mode is faster
        rows.append(
            {
                "skeleton_id": sid,
                "bucket": r.get("bucket"),
                "member_count": r.get("member_count"),
                "per_hash_ns": ph_ns,
                "skelot_ns": sk_ns,
                "all_variant_ns": av_ns,
                "skelot_speedup": ph_ns / sk_ns,
                "all_variant_speedup": ph_ns / av_ns,
                "av_minus_sk_speedup": (ph_ns / av_ns) - (ph_ns / sk_ns),
                "datatable_cost_pct": (sk_ns / av_ns - 1.0) * 100.0,
                "num_invariant_av": r["all_variant"]["num_invariant"],
                "num_variant_av": r["all_variant"]["num_variant"],
                "num_variant_sk": r["skelot"]["num_variant"],
            }
        )

    sk_speedups = np.array([r["skelot_speedup"] for r in rows])
    av_speedups = np.array([r["all_variant_speedup"] for r in rows])
    dt_costs = np.array([r["datatable_cost_pct"] for r in rows])

    summary = {
        "n_families": len(rows),
        "skelot_speedup": {
            "median": float(np.median(sk_speedups)),
            "mean": float(np.mean(sk_speedups)),
            "p25": float(np.percentile(sk_speedups, 25)),
            "p75": float(np.percentile(sk_speedups, 75)),
        },
        "all_variant_speedup_locality_only": {
            "median": float(np.median(av_speedups)),
            "mean": float(np.mean(av_speedups)),
            "p25": float(np.percentile(av_speedups, 25)),
            "p75": float(np.percentile(av_speedups, 75)),
        },
        "datatable_cost_pct_skelot_minus_all_variant": {
            "median": float(np.median(dt_costs)),
            "mean": float(np.mean(dt_costs)),
            "p25": float(np.percentile(dt_costs, 25)),
            "p75": float(np.percentile(dt_costs, 75)),
        },
        "av_faster_than_skelot": int(np.sum(av_speedups > sk_speedups)),
        "sk_faster_than_av": int(np.sum(sk_speedups > av_speedups)),
        "tied_within_1pct": int(
            np.sum(np.abs(av_speedups - sk_speedups) < 0.01)
        ),
        "decomposition_explanation": (
            "skelot_speedup = locality_benefit (A) - datatable_cost (B). "
            "all_variant_speedup approximates locality_benefit alone "
            "(same single-.so structure as skelot, but no data-table). "
            "Median datatable_cost_pct quantifies B; "
            "median all_variant_speedup quantifies A."
        ),
        "rows": rows,
    }

    (OUT / "critique_response_p3.json").write_text(json.dumps(summary, indent=2))
    return summary


def problem4_characterize_slower():
    """The 49 contracts where skelot is slower than per_hash in
    perf_per_target_per_contract.csv.  Look at family_size, bucket,
    variant ratio, protocol_tag distributions vs the 281 faster ones."""
    perf = load_perf_per_contract()
    joined = load_joined()
    fams = load_av_families()

    slower = []
    faster = []
    for ch, p in perf.items():
        ratio = float(p["cyc_per_call_ratio_skelot_over_ph"])
        j = joined.get(ch)
        if j is None:
            continue
        skel_short = j["skeleton_hash"]
        av = fams.get(skel_short)
        rec = {
            "code_hash": ch,
            "cyc_ratio": ratio,
            "ph_calls": int(p["ph_calls"]),
            "ph_cycles_per_call": float(p["ph_cycles_per_call"]),
            "family_size": int(j["family_size"]),
            "skeleton_len": int(j["skeleton_len"]) if j.get("skeleton_len") else None,
            "bucket": j["bucket"],
            "av_variant_ratio": (
                float(j["av_variant_ratio"]) if j.get("av_variant_ratio") else None
            ),
            "skeleton_hash": skel_short,
            "protocol_tag": j.get("protocol_tag") or "",
            "speedup_ratio_orig": float(j["speedup_ratio"]),
        }
        if ratio > 1.0:
            slower.append(rec)
        else:
            faster.append(rec)

    def stats_block(rows, key, kind="median"):
        vals = [r[key] for r in rows if r[key] is not None]
        if not vals:
            return None
        if kind == "median":
            return float(np.median(vals))
        if kind == "mean":
            return float(np.mean(vals))
        if kind == "p25":
            return float(np.percentile(vals, 25))
        if kind == "p75":
            return float(np.percentile(vals, 75))

    def bucket_dist(rows):
        c = defaultdict(int)
        for r in rows:
            c[r["bucket"]] += 1
        return dict(c)

    def family_size_dist(rows):
        bins = [("1", 1, 1), ("2-9", 2, 9), ("10-99", 10, 99), ("100+", 100, 10**9)]
        c = {b[0]: 0 for b in bins}
        for r in rows:
            for name, lo, hi in bins:
                if lo <= r["family_size"] <= hi:
                    c[name] += 1
                    break
        return c

    def protocol_dist(rows, top_k=8):
        c = defaultdict(int)
        for r in rows:
            c[r["protocol_tag"] or "(none)"] += 1
        return dict(sorted(c.items(), key=lambda kv: -kv[1])[:top_k])

    summary = {
        "n_slower": len(slower),
        "n_faster": len(faster),
        "slower_pct": 100.0 * len(slower) / max(1, len(slower) + len(faster)),
        "median_family_size": {
            "slower": stats_block(slower, "family_size"),
            "faster": stats_block(faster, "family_size"),
        },
        "median_skeleton_len": {
            "slower": stats_block(slower, "skeleton_len"),
            "faster": stats_block(faster, "skeleton_len"),
        },
        "median_av_variant_ratio": {
            "slower": stats_block(slower, "av_variant_ratio"),
            "faster": stats_block(faster, "av_variant_ratio"),
        },
        "median_ph_calls": {
            "slower": stats_block(slower, "ph_calls"),
            "faster": stats_block(faster, "ph_calls"),
        },
        "median_ph_cycles_per_call": {
            "slower": stats_block(slower, "ph_cycles_per_call"),
            "faster": stats_block(faster, "ph_cycles_per_call"),
        },
        "bucket_dist": {
            "slower": bucket_dist(slower),
            "faster": bucket_dist(faster),
        },
        "family_size_dist": {
            "slower": family_size_dist(slower),
            "faster": family_size_dist(faster),
        },
        "protocol_dist_slower_top": protocol_dist(slower),
    }

    # Mann-Whitney one-sided: do slower contracts have smaller family_size?
    slower_fs = [r["family_size"] for r in slower]
    faster_fs = [r["family_size"] for r in faster]
    if slower_fs and faster_fs:
        u = sstats.mannwhitneyu(slower_fs, faster_fs, alternative="less")
        summary["mwu_family_size_slower_lt_faster"] = {
            "U": float(u.statistic),
            "p": float(u.pvalue),
        }
    slower_calls = [r["ph_calls"] for r in slower]
    faster_calls = [r["ph_calls"] for r in faster]
    if slower_calls and faster_calls:
        u = sstats.mannwhitneyu(slower_calls, faster_calls, alternative="less")
        summary["mwu_ph_calls_slower_lt_faster"] = {
            "U": float(u.statistic),
            "p": float(u.pvalue),
        }

    with open(OUT / "critique_response_p4.csv", "w", newline="") as f:
        slower_sorted = sorted(slower, key=lambda r: -r["cyc_ratio"])
        w = csv.DictWriter(f, fieldnames=list(slower_sorted[0].keys()))
        w.writeheader()
        w.writerows(slower_sorted)

    return summary


def main():
    p2 = problem2_cycle_vs_wall()
    p3 = problem3_locality_vs_datatable()
    p4 = problem4_characterize_slower()

    out = {
        "problem_2_cycle_vs_wall": p2,
        "problem_3_locality_vs_datatable_summary": {
            k: v for k, v in p3.items() if k != "rows"
        },
        "problem_4_slower_characterization": p4,
    }
    (OUT / "critique_response_summary.json").write_text(json.dumps(out, indent=2))

    # Pretty print
    print("=== Problem 2: cycle ratio vs wall ratio per contract ===")
    print(f"  n contracts joined: {p2['n']}")
    print(f"  median cycle ratio (skelot/per_hash): {p2['median_cyc_ratio']:.4f}  → speedup {p2['implied_cyc_speedup']:.4f}x")
    print(f"  median wall  ratio (skelot/per_hash): {p2['median_wall_ratio']:.4f}  → speedup {p2['implied_wall_speedup']:.4f}x")
    print(f"  Pearson  r between cycle and wall ratio: {p2['pearson_cyc_vs_wall']['r']:.4f} (p={p2['pearson_cyc_vs_wall']['p']:.2e})")
    print(f"  Spearman ρ between cycle and wall ratio: {p2['spearman_cyc_vs_wall']['rho']:.4f} (p={p2['spearman_cyc_vs_wall']['p']:.2e})")
    print(f"  mean (wall_ratio - cyc_ratio) = {p2['mean_wall_minus_cyc']:.4f} ± {p2['stdev_wall_minus_cyc']:.4f}")
    print()
    print("=== Problem 3: locality (A) vs data-table cost (B) ===")
    sks = p3["skelot_speedup"]
    avs = p3["all_variant_speedup_locality_only"]
    dt = p3["datatable_cost_pct_skelot_minus_all_variant"]
    print(f"  n families (each is one skeleton family): {p3['n_families']}")
    print(f"  skelot speedup (A+B):           median {sks['median']:.4f}  IQR [{sks['p25']:.4f}, {sks['p75']:.4f}]")
    print(f"  all_variant speedup (A alone):  median {avs['median']:.4f}  IQR [{avs['p25']:.4f}, {avs['p75']:.4f}]")
    print(f"  datatable cost B (skelot vs all_variant): median {dt['median']:.3f}%  IQR [{dt['p25']:.3f}%, {dt['p75']:.3f}%]")
    print(f"  all_variant faster than skelot in {p3['av_faster_than_skelot']}/{p3['n_families']} families")
    print()
    print("=== Problem 4: characterization of skelot-slower contracts ===")
    print(f"  slower (cyc_ratio>1): {p4['n_slower']}/{p4['n_slower']+p4['n_faster']} = {p4['slower_pct']:.1f}%")
    print(f"  median family_size: slower={p4['median_family_size']['slower']}  faster={p4['median_family_size']['faster']}")
    print(f"  median skeleton_len: slower={p4['median_skeleton_len']['slower']}  faster={p4['median_skeleton_len']['faster']}")
    print(f"  median av_variant_ratio: slower={p4['median_av_variant_ratio']['slower']}  faster={p4['median_av_variant_ratio']['faster']}")
    print(f"  median ph_calls (count): slower={p4['median_ph_calls']['slower']}  faster={p4['median_ph_calls']['faster']}")
    print(f"  median ph_cycles_per_call: slower={p4['median_ph_cycles_per_call']['slower']:.0f}  faster={p4['median_ph_cycles_per_call']['faster']:.0f}")
    print(f"  bucket distribution: slower={p4['bucket_dist']['slower']}  faster={p4['bucket_dist']['faster']}")
    print(f"  family_size distribution: slower={p4['family_size_dist']['slower']}  faster={p4['family_size_dist']['faster']}")
    if "mwu_family_size_slower_lt_faster" in p4:
        m = p4["mwu_family_size_slower_lt_faster"]
        print(f"  MWU 'slower has smaller family_size': U={m['U']:.0f}, p={m['p']:.3e}")
    if "mwu_ph_calls_slower_lt_faster" in p4:
        m = p4["mwu_ph_calls_slower_lt_faster"]
        print(f"  MWU 'slower has fewer ph_calls':       U={m['U']:.0f}, p={m['p']:.3e}")


if __name__ == "__main__":
    main()
