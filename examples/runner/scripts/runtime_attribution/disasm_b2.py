"""Stage B disasm-level comparison for the largest skeleton family.

Compares per-bytecode .text size (per_hash mode) against the shared SkelOT
.text for skel_f812d76162f34d9a (Uniswap V3 Pool, 3,323 family members,
median per-contract speedup 1.40 in skeleton_vs_perhash.json).

Output:
  text_size_comparison.csv
  text_size_comparison.json

This deliberately uses the on-disk .so files only — no recompilation, no
benchmark re-run. The signal is the per-instance .text bloat introduced
by SkelOT's variant-PUSH triple, which directly tests whether per-contract
runtime cost increases under SkelOT.
"""
from __future__ import annotations

import csv
import json
import os
import subprocess
from pathlib import Path


def _env_path(name: str, what: str) -> Path:
    v = os.environ.get(name)
    if not v:
        raise SystemExit(
            f"Set {name} environment variable ({what}). See README.md in this directory."
        )
    return Path(v)


DATA = _env_path("INPUT_DATA_DIR", "directory holding skeleton_mapping.tsv")
PH = _env_path("PERHASH_CACHE", "per-hash .so cache directory")
SK = _env_path("SKELOT_CACHE", "skelot .so cache directory")
OUT = Path(os.environ.get("OUT_DIR") or Path(__file__).resolve().parent / "output")
OUT.mkdir(parents=True, exist_ok=True)

TARGET = "f812d76162f34d9a"
N_SAMPLE = 200


def text_size(path: Path) -> int:
    out = subprocess.check_output(["size", str(path)]).decode()
    return int(out.strip().split("\n")[1].split()[0])


def main():
    members = []
    with open(DATA / "skeleton_mapping.tsv") as f:
        for line in f:
            short, full = line.rstrip("\n").split("\t")
            if short == TARGET:
                members.append(full.lower().replace("0x", ""))

    present = [m for m in members if (PH / f"{m}__prague__o3.so").exists()]
    sk_so = SK / f"skel_{TARGET}__prague__o3.so"
    assert sk_so.exists(), sk_so
    sk_text = text_size(sk_so)

    sample = present[:N_SAMPLE]
    rows = []
    for m in sample:
        ph_so = PH / f"{m}__prague__o3.so"
        rows.append(
            {
                "member_short": m[:12],
                "ph_so_bytes": ph_so.stat().st_size,
                "ph_text_bytes": text_size(ph_so),
            }
        )
    ph_text_min = min(r["ph_text_bytes"] for r in rows)
    ph_text_max = max(r["ph_text_bytes"] for r in rows)
    ph_text_mean = sum(r["ph_text_bytes"] for r in rows) / len(rows)

    summary = {
        "skeleton_id": TARGET,
        "family_size_total": len(members),
        "members_present_in_per_hash_cache": len(present),
        "members_sampled_for_size": len(sample),
        "ph_text_min": ph_text_min,
        "ph_text_max": ph_text_max,
        "ph_text_mean": ph_text_mean,
        "skelot_shared_text_bytes": sk_text,
        "skelot_per_instance_text_overhead_bytes": sk_text - ph_text_min,
        "skelot_per_instance_text_overhead_pct": (sk_text - ph_text_min) / ph_text_min * 100,
        "ondisk_text_savings_bytes_full_family": (
            len(members) * ph_text_min - sk_text
        ),
    }

    # Cross-check non-identical content of two members (variants are PUSH constants only)
    a, b = present[0], present[1]
    fa = PH / f"{a}__prague__o3.so"
    fb = PH / f"{b}__prague__o3.so"
    diff = subprocess.run(
        ["cmp", "-l", str(fa), str(fb)], capture_output=True
    )
    diff_byte_count = len(diff.stdout.splitlines())
    summary["per_hash_a_b_byte_diff_count"] = diff_byte_count

    with open(OUT / "text_size_comparison.csv", "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=list(rows[0].keys()))
        w.writeheader()
        w.writerows(rows)
    (OUT / "text_size_comparison.json").write_text(json.dumps(summary, indent=2))

    # Stdout
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
