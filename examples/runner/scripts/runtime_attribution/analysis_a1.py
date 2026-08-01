"""Stage A reanalysis for SkelOT vs per-hash 1.17x median speedup attribution.

Single-shot: every number in runtime_attribution_preliminary.md is reproduced
by running this script. No re-execution of benchmarks; all inputs are
read-only JSON/TSV files under skelot-paper/data/.

Outputs (under this script's directory):
  joined_per_contract.csv
  join_summary.json
  signal_*_scatter.png            (one per Spearman test)
  family_size_buckets.csv
  skeleton_len_buckets.csv
  variant_push_summary.csv
  signal_*.json                    (machine-readable stats per signal)

Run:
  python3 analysis_a1.py
"""
from __future__ import annotations

import json
import math
import csv
from pathlib import Path
from collections import defaultdict

import numpy as np
import matplotlib
import os

matplotlib.use("Agg")
import matplotlib.pyplot as plt
from scipy import stats as sstats


def _env_path(name: str, what: str) -> Path:
    v = os.environ.get(name)
    if not v:
        raise SystemExit(
            f"Set {name} environment variable ({what}). See README.md in this directory."
        )
    return Path(v)


DATA = _env_path(
    "INPUT_DATA_DIR",
    "directory of read-only paper inputs: skeleton_vs_perhash.json, "
    "compile_bench.json, dataset_scan.json, skeleton_families.json, "
    "ablation_allvariant.families.jsonl, skeleton_mapping.tsv",
)
OUT = Path(os.environ.get("OUT_DIR") or Path(__file__).resolve().parent / "output")
OUT.mkdir(parents=True, exist_ok=True)


def bucket_for_raw_len(n: int) -> str:
    # Mirrors revmc/examples/runner/src/skelot_common.rs::bucket_for_raw_len
    if n <= 99:
        return "tiny"
    if n <= 999:
        return "small"
    if n <= 4999:
        return "medium"
    if n <= 14999:
        return "large"
    return "huge"


def load_inputs():
    svp = json.loads((DATA / "skeleton_vs_perhash.json").read_text())
    cb = json.loads((DATA / "compile_bench.json").read_text())
    fams = json.loads((DATA / "skeleton_families.json").read_text())["families"]

    ph_rec = next(r for r in cb["records"] if r["mode"] == "per_hash")
    sk_rec = next(r for r in cb["records"] if r["mode"] == "skelot")

    ph_o_by_ch = dict(zip(ph_rec["compile_unit_hashes"], ph_rec["per_unit_o_bytes"]))
    sk_o_by_repch = dict(zip(sk_rec["compile_unit_hashes"], sk_rec["per_unit_o_bytes"]))

    sk_repch_set = set(sk_rec["compile_unit_hashes"])
    short_to_repch: dict[str, str] = {}
    with open(DATA / "skeleton_mapping.tsv") as f:
        for line in f:
            short, code_hash = line.rstrip("\n").split("\t")
            if code_hash in sk_repch_set:
                short_to_repch[short] = code_hash

    sk_o_by_short = {s: sk_o_by_repch[ch] for s, ch in short_to_repch.items()}
    fam_by_short = {f["skeleton_hash"]: f for f in fams}

    # ablation_allvariant.families.jsonl: short skel hash -> {num_invariant, num_variant}
    av_by_short: dict[str, dict] = {}
    with open(DATA / "ablation_allvariant.families.jsonl") as f:
        for line in f:
            r = json.loads(line)
            short = r["skeleton_id"]
            av_by_short[short] = {
                "av_num_invariant": r["all_variant"]["num_invariant"],
                "av_num_variant": r["all_variant"]["num_variant"],
                "ph_num_invariant": r["per_hash"]["num_invariant"],
                "skelot_num_invariant": r["skelot"]["num_invariant"],
                "skelot_num_variant": r["skelot"]["num_variant"],
                "av_so_bytes": r["all_variant"]["so_bytes"],
                "ph_so_bytes": r["per_hash"]["so_bytes"],
                "skelot_so_bytes": r["skelot"]["so_bytes"],
            }

    return svp, ph_o_by_ch, sk_o_by_short, fam_by_short, av_by_short


def build_table():
    svp, ph_o_by_ch, sk_o_by_short, fam_by_short, av_by_short = load_inputs()
    rows = []
    for pc in svp["per_contract"]:
        code_hash = pc["code_hash"].lower()
        short = pc["skeleton_hash"].replace("skel_", "")
        fam = fam_by_short.get(short)
        av = av_by_short.get(short)
        # `skeleton_len` has PUSH immediates stripped, so it must NOT be
        # re-bucketed here — Stage A's `bucket` is computed from the raw
        # bytecode length and is the stratum the paper reports.
        skeleton_len = fam["skeleton_len"] if fam else None
        bucket = fam["bucket"] if fam else None
        rec = {
            "code_hash": code_hash,
            "skeleton_hash": short,
            "family_size": pc["family_size"],
            "blocks_seen": pc["blocks_seen"],
            "mean_per_hash_us": pc["mean_per_hash_us"],
            "mean_skeleton_us": pc["mean_skeleton_us"],
            "delta_us": pc["delta_us"],
            "speedup_ratio": pc["speedup_ratio"],
            "verdict": pc["verdict"],
            "naive_o_bytes": ph_o_by_ch.get(code_hash),
            "skelot_o_bytes": sk_o_by_short.get(short),
            "o_bytes_ratio": (
                ph_o_by_ch.get(code_hash) / sk_o_by_short.get(short)
                if ph_o_by_ch.get(code_hash) is not None
                and sk_o_by_short.get(short) is not None
                and sk_o_by_short.get(short) > 0
                else None
            ),
            "skeleton_len": skeleton_len,
            "bucket": bucket,
            "fam_canonical_size": fam["family_size"] if fam else None,
            "protocol_tag": fam["protocol_tag"] if fam else None,
            "av_num_invariant": av["av_num_invariant"] if av else None,
            "av_num_variant": av["av_num_variant"] if av else None,
            "av_variant_ratio": (
                av["av_num_variant"]
                / max(av["av_num_invariant"] + av["av_num_variant"], 1)
                if av
                else None
            ),
        }
        rows.append(rec)
    return rows


def write_csv(rows, path):
    with open(path, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=list(rows[0].keys()))
        w.writeheader()
        w.writerows(rows)


def spearman(xs, ys):
    xs = np.asarray(xs, dtype=float)
    ys = np.asarray(ys, dtype=float)
    mask = np.isfinite(xs) & np.isfinite(ys)
    xs, ys = xs[mask], ys[mask]
    rho, p = sstats.spearmanr(xs, ys)
    return {"n": int(mask.sum()), "rho": float(rho), "p": float(p)}


def quartile_summary(values):
    arr = np.asarray(values, dtype=float)
    arr = arr[np.isfinite(arr)]
    if len(arr) == 0:
        return {"n": 0, "median": None, "q25": None, "q75": None, "min": None, "max": None}
    return {
        "n": int(len(arr)),
        "median": float(np.median(arr)),
        "q25": float(np.quantile(arr, 0.25)),
        "q75": float(np.quantile(arr, 0.75)),
        "min": float(arr.min()),
        "max": float(arr.max()),
    }


def scatter(x, y, xlabel, ylabel, title, out_path, log_x=False, alpha=0.25):
    plt.figure(figsize=(6, 4))
    plt.scatter(x, y, s=8, alpha=alpha)
    if log_x:
        plt.xscale("log")
    plt.axhline(1.0, color="gray", lw=0.5)
    plt.xlabel(xlabel)
    plt.ylabel(ylabel)
    plt.title(title, fontsize=10)
    plt.tight_layout()
    plt.savefig(out_path, dpi=120)
    plt.close()


def main():
    rows = build_table()
    write_csv(rows, OUT / "joined_per_contract.csv")

    # ----- A1: row count + missingness -----
    cols = list(rows[0].keys())
    missing = {k: sum(1 for r in rows if r[k] is None) for k in cols}
    summary = {
        "n_rows": len(rows),
        "missing": missing,
        "verdict_counts": dict(
            (v, sum(1 for r in rows if r["verdict"] == v))
            for v in {r["verdict"] for r in rows}
        ),
    }
    (OUT / "join_summary.json").write_text(json.dumps(summary, indent=2))
    print(f"[A1] rows={len(rows)} missing={missing}")

    # Convenience numpy arrays
    spd = np.array([r["speedup_ratio"] for r in rows], dtype=float)
    fs = np.array([r["family_size"] for r in rows], dtype=float)
    bl = np.array([r["skeleton_len"] for r in rows], dtype=float)
    no = np.array(
        [r["naive_o_bytes"] if r["naive_o_bytes"] is not None else np.nan for r in rows],
        dtype=float,
    )
    so = np.array(
        [r["skelot_o_bytes"] if r["skelot_o_bytes"] is not None else np.nan for r in rows],
        dtype=float,
    )
    obr = np.array(
        [r["o_bytes_ratio"] if r["o_bytes_ratio"] is not None else np.nan for r in rows],
        dtype=float,
    )
    av_var = np.array(
        [r["av_num_variant"] if r["av_num_variant"] is not None else np.nan for r in rows],
        dtype=float,
    )
    av_var_ratio = np.array(
        [r["av_variant_ratio"] if r["av_variant_ratio"] is not None else np.nan for r in rows],
        dtype=float,
    )

    # ----- A2: four Spearman correlations -----
    signals = {}
    signals["family_size"] = {
        "spearman_speedup_vs_family_size": spearman(fs, spd),
        "spearman_speedup_vs_log_family_size": spearman(np.log10(fs), spd),
    }
    signals["skeleton_len"] = {
        "spearman_speedup_vs_skeleton_len": spearman(bl, spd),
    }
    signals["naive_o_bytes"] = {
        "spearman_speedup_vs_naive_o_bytes": spearman(no, spd),
    }
    signals["o_bytes_ratio"] = {
        "spearman_speedup_vs_o_bytes_ratio": spearman(obr, spd),
    }
    print("[A2] Spearman:")
    for k, v in signals.items():
        for name, s in v.items():
            print(f"   {name}: n={s['n']} rho={s['rho']:.4f} p={s['p']:.3e}")

    # Plots for A2
    scatter(
        fs,
        spd,
        "family_size (log)",
        "speedup_ratio",
        f"speedup vs family_size  (Spearman rho={signals['family_size']['spearman_speedup_vs_family_size']['rho']:.3f}, n={signals['family_size']['spearman_speedup_vs_family_size']['n']})",
        OUT / "signal_family_size_scatter.png",
        log_x=True,
    )
    scatter(
        bl,
        spd,
        "skeleton_len (log)",
        "speedup_ratio",
        f"speedup vs skeleton_len  (Spearman rho={signals['skeleton_len']['spearman_speedup_vs_skeleton_len']['rho']:.3f}, n={signals['skeleton_len']['spearman_speedup_vs_skeleton_len']['n']})",
        OUT / "signal_skeleton_len_scatter.png",
        log_x=True,
    )
    scatter(
        no,
        spd,
        "naive_o_bytes (log)",
        "speedup_ratio",
        f"speedup vs naive_o_bytes  (Spearman rho={signals['naive_o_bytes']['spearman_speedup_vs_naive_o_bytes']['rho']:.3f}, n={signals['naive_o_bytes']['spearman_speedup_vs_naive_o_bytes']['n']})",
        OUT / "signal_naive_o_bytes_scatter.png",
        log_x=True,
    )
    scatter(
        obr,
        spd,
        "naive_o_bytes / skelot_o_bytes (log)",
        "speedup_ratio",
        f"speedup vs o_bytes_ratio  (Spearman rho={signals['o_bytes_ratio']['spearman_speedup_vs_o_bytes_ratio']['rho']:.3f}, n={signals['o_bytes_ratio']['spearman_speedup_vs_o_bytes_ratio']['n']})",
        OUT / "signal_o_bytes_ratio_scatter.png",
        log_x=True,
    )

    # ----- A3a: family_size buckets -----
    fs_bins = [
        ("1", lambda v: v == 1),
        ("2-9", lambda v: 2 <= v <= 9),
        ("10-99", lambda v: 10 <= v <= 99),
        ("100+", lambda v: v >= 100),
    ]
    fs_table = []
    for name, pred in fs_bins:
        sel = [r["speedup_ratio"] for r in rows if pred(r["family_size"])]
        q = quartile_summary(sel)
        q["bin"] = name
        fs_table.append(q)
    write_csv(fs_table, OUT / "family_size_buckets.csv")

    # ----- A3b: skeleton_len buckets -----
    bl_table = []
    for name in ["tiny", "small", "medium", "large", "huge"]:
        sel = [r["speedup_ratio"] for r in rows if r["bucket"] == name]
        q = quartile_summary(sel)
        q["bin"] = name
        bl_table.append(q)
    write_csv(bl_table, OUT / "skeleton_len_buckets.csv")

    def _bucket_plot(table, xlabel, out_name):
        present = [t for t in table if t["n"] > 0]
        if not present:
            return
        pos = np.arange(len(present))
        medians = [t["median"] for t in present]
        q25 = [t["q25"] for t in present]
        q75 = [t["q75"] for t in present]
        yerr = [
            [m - lo for m, lo in zip(medians, q25)],
            [hi - m for m, hi in zip(medians, q75)],
        ]
        plt.figure(figsize=(6, 4))
        plt.errorbar(pos, medians, yerr=yerr, fmt="o-", capsize=4)
        plt.xticks(pos, [f"{t['bin']}\n(n={t['n']})" for t in present])
        plt.axhline(1.0, color="gray", lw=0.5)
        plt.xlabel(xlabel)
        plt.ylabel("speedup_ratio (median, IQR)")
        plt.title(
            f"speedup by {xlabel}  (n_total={sum(t['n'] for t in table)})"
        )
        plt.tight_layout()
        plt.savefig(OUT / out_name, dpi=120)
        plt.close()

    _bucket_plot(fs_table, "family_size bucket", "family_size_bucket_median.png")
    _bucket_plot(bl_table, "bytecode bucket (raw_len)", "bytecode_bucket_median.png")

    # ----- A4: variant-PUSH analysis -----
    var_signals = {
        "spearman_speedup_vs_av_num_variant": spearman(av_var, spd),
        "spearman_speedup_vs_av_variant_ratio": spearman(av_var_ratio, spd),
    }

    # 68 skeleton_slower contracts: are they on the variant-heavy side?
    slower = [r for r in rows if r["verdict"] == "SKELETON_SLOWER"]
    others = [r for r in rows if r["verdict"] != "SKELETON_SLOWER"]
    slower_var = [r["av_num_variant"] for r in slower if r["av_num_variant"] is not None]
    others_var = [r["av_num_variant"] for r in others if r["av_num_variant"] is not None]
    slower_var_ratio = [
        r["av_variant_ratio"] for r in slower if r["av_variant_ratio"] is not None
    ]
    others_var_ratio = [
        r["av_variant_ratio"] for r in others if r["av_variant_ratio"] is not None
    ]

    if len(slower_var) > 0 and len(others_var) > 0:
        u = sstats.mannwhitneyu(slower_var, others_var, alternative="greater")
        var_signals["mwu_slower_vs_others_av_num_variant"] = {
            "n_slower": len(slower_var),
            "n_others": len(others_var),
            "U": float(u.statistic),
            "p_one_sided_greater": float(u.pvalue),
            "median_slower": float(np.median(slower_var)),
            "median_others": float(np.median(others_var)),
        }
    if len(slower_var_ratio) > 0 and len(others_var_ratio) > 0:
        u2 = sstats.mannwhitneyu(slower_var_ratio, others_var_ratio, alternative="greater")
        var_signals["mwu_slower_vs_others_av_variant_ratio"] = {
            "n_slower": len(slower_var_ratio),
            "n_others": len(others_var_ratio),
            "U": float(u2.statistic),
            "p_one_sided_greater": float(u2.pvalue),
            "median_slower": float(np.median(slower_var_ratio)),
            "median_others": float(np.median(others_var_ratio)),
        }

    write_csv(
        [
            {
                "group": "skeleton_slower",
                "n": len(slower_var),
                "median_av_num_variant": (
                    float(np.median(slower_var)) if slower_var else None
                ),
                "median_av_variant_ratio": (
                    float(np.median(slower_var_ratio)) if slower_var_ratio else None
                ),
            },
            {
                "group": "others",
                "n": len(others_var),
                "median_av_num_variant": (
                    float(np.median(others_var)) if others_var else None
                ),
                "median_av_variant_ratio": (
                    float(np.median(others_var_ratio)) if others_var_ratio else None
                ),
            },
        ],
        OUT / "variant_push_summary.csv",
    )

    # Scatter: variant_ratio vs speedup
    scatter(
        av_var_ratio,
        spd,
        "av_variant_ratio (= num_variant / (num_invariant + num_variant))",
        "speedup_ratio",
        f"speedup vs av_variant_ratio (Spearman rho={var_signals['spearman_speedup_vs_av_variant_ratio']['rho']:.3f}, n={var_signals['spearman_speedup_vs_av_variant_ratio']['n']})",
        OUT / "signal_variant_ratio_scatter.png",
    )

    # ----- A5 confound analysis: partial correlations + within-bucket trends -----
    def partial_spearman(x, y, z):
        x = np.asarray(x, dtype=float)
        y = np.asarray(y, dtype=float)
        z = np.asarray(z, dtype=float)
        mask = np.isfinite(x) & np.isfinite(y) & np.isfinite(z)
        x, y, z = x[mask], y[mask], z[mask]
        rx = sstats.rankdata(x)
        ry = sstats.rankdata(y)
        rz = sstats.rankdata(z)

        def resid(a, b):
            slope, intercept, *_ = sstats.linregress(b, a)
            return a - (slope * b + intercept)

        rx_res = resid(rx, rz)
        ry_res = resid(ry, rz)
        rho, p = sstats.pearsonr(rx_res, ry_res)
        return {"n": int(mask.sum()), "rho": float(rho), "p": float(p)}

    confound = {
        "spearman_log_family_size_vs_variant_ratio": spearman(np.log10(fs), av_var_ratio),
        "spearman_skeleton_len_vs_variant_ratio": spearman(bl, av_var_ratio),
        "spearman_family_size_vs_skeleton_len": spearman(fs, bl),
        "partial_variant_ratio_vs_speedup_given_log_fs": partial_spearman(
            av_var_ratio, spd, np.log10(fs)
        ),
        "partial_skeleton_len_vs_speedup_given_log_fs": partial_spearman(
            bl, spd, np.log10(fs)
        ),
    }
    print("[A5] confound:")
    for k, v in confound.items():
        print("  ", k, v)

    within_bucket = []
    for bk in ["tiny", "small", "medium", "large", "huge"]:
        sub = [r for r in rows if r["bucket"] == bk]
        if len(sub) < 10:
            continue
        s_fs = np.array([r["family_size"] for r in sub], dtype=float)
        s_spd = np.array([r["speedup_ratio"] for r in sub], dtype=float)
        s = spearman(np.log10(s_fs), s_spd)
        s["bucket"] = bk
        within_bucket.append(s)
    print("[A5] within-bucket spearman(log_family_size, speedup):")
    for s in within_bucket:
        print("  ", s)

    # Save aggregated signal stats
    (OUT / "signals.json").write_text(
        json.dumps(
            {
                "A2_correlations": signals,
                "A4_variant_push": var_signals,
                "A3_family_size_buckets": fs_table,
                "A3_bytecode_buckets": bl_table,
                "A5_confound_partials": confound,
                "A5_within_bucket_family_size_trend": within_bucket,
            },
            indent=2,
        )
    )

    # Compact stdout dump for sanity
    print("[A3] family_size buckets:")
    for t in fs_table:
        print("  ", t)
    print("[A3] bytecode buckets:")
    for t in bl_table:
        print("  ", t)
    print("[A4] variant push:")
    for k, v in var_signals.items():
        print("  ", k, v)


if __name__ == "__main__":
    main()
