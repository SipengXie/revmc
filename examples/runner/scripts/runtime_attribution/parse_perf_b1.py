"""Parse the four perf-stat .stat files produced by perf_b1.sh and emit
a side-by-side comparison + per-event ratios in skeleton vs per_hash mode.

The bench runs identical setup phases (Phase 1-4 load both .so caches and
build the dispatch maps), and `perf stat -D 35000` only enables counters
35 s into the run, after Phase 1-4 are done. Whatever perf measures is
attributable to the measurement loop (Phase 5), which differs only in
`--only-mode`.

Run after perf_b1.sh finishes:
  python3 parse_perf_b1.py
"""
from __future__ import annotations

import csv
import os
import json
from pathlib import Path

OUT = Path(os.environ.get("OUT_DIR") or Path(__file__).resolve().parent / "output")
OUT.mkdir(parents=True, exist_ok=True)
EVENT_SETS = ["icache", "other"]
MODES = ["per_hash", "skeleton"]


def parse_perf_stat(path: Path) -> dict[str, float]:
    """Parse perf stat -x, output.

    Fields: counter value, unit, event name, run time (ns), percentage of
    time the counter was running, optional derived metric, metric unit.
    """
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
        # Track running fraction so multiplexing is visible
        running_pct = parts[4] if len(parts) > 4 else ""
        counts[event] = count
        counts[f"{event}__running_pct"] = float(running_pct) if running_pct else 100.0
    return counts


def main():
    # perf_b1.sh writes perf_<mode>_<event_set>.<RUN_TAG>.stat; fall back to
    # the legacy untagged name for stat files produced before RUN_TAG existed.
    run_tag = os.environ.get("RUN_TAG", "r1")
    grid: dict[str, dict[str, dict[str, float]]] = {}
    for mode in MODES:
        grid[mode] = {}
        for es in EVENT_SETS:
            stat = OUT / f"perf_{mode}_{es}.{run_tag}.stat"
            if not stat.exists():
                stat = OUT / f"perf_{mode}_{es}.stat"
            grid[mode][es] = parse_perf_stat(stat)

    # Aggregate per-event into a single per-mode flat dict
    flat: dict[str, dict[str, float]] = {}
    for mode in MODES:
        flat[mode] = {}
        for es in EVENT_SETS:
            for k, v in grid[mode][es].items():
                flat[mode][k] = v

    # Build comparison rows
    events_of_interest = [
        "cycles",
        "instructions",
        "L1-icache-load-misses",
        "L1-icache-loads",
        "iTLB-load-misses",
        "branch-misses",
        "branches",
    ]
    rows = []
    for ev in events_of_interest:
        ph = flat["per_hash"].get(ev)
        sk = flat["skeleton"].get(ev)
        ratio = sk / ph if (ph and sk) else None
        rows.append(
            {
                "event": ev,
                "per_hash": ph,
                "skeleton": sk,
                "skelot_div_per_hash": ratio,
                "delta_count": (sk - ph) if (ph is not None and sk is not None) else None,
                "ph_running_pct": flat["per_hash"].get(f"{ev}__running_pct"),
                "sk_running_pct": flat["skeleton"].get(f"{ev}__running_pct"),
            }
        )

    # Derived: cycles per instruction, miss rates
    def derived(mode):
        d = flat[mode]
        cyc = d.get("cycles", 0)
        ins = d.get("instructions", 0)
        if not cyc or not ins:
            return {}
        return {
            "cycles_per_instr": cyc / ins,
            "L1i_miss_per_kinstr": (d.get("L1-icache-load-misses", 0) / ins) * 1000.0,
            "L1i_miss_pct_of_loads": (
                (d.get("L1-icache-load-misses", 0) / d["L1-icache-loads"]) * 100.0
                if d.get("L1-icache-loads")
                else None
            ),
            "iTLB_miss_per_kinstr": (d.get("iTLB-load-misses", 0) / ins) * 1000.0,
            "branch_miss_per_kbranch": (
                (d.get("branch-misses", 0) / d["branches"]) * 1000.0
                if d.get("branches")
                else None
            ),
        }

    summary = {
        "events": rows,
        "derived": {
            "per_hash": derived("per_hash"),
            "skeleton": derived("skeleton"),
        },
    }

    # Skeleton vs per_hash ratios for derived
    sd = summary["derived"]["skeleton"]
    pd = summary["derived"]["per_hash"]
    summary["derived_ratios_skel_over_ph"] = {
        k: (sd.get(k) / pd[k] if pd.get(k) else None)
        for k in sd
        if isinstance(sd.get(k), (int, float)) and pd.get(k)
    }

    (OUT / "perf_summary.json").write_text(json.dumps(summary, indent=2))

    # CSV
    with open(OUT / "perf_summary.csv", "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=list(rows[0].keys()))
        w.writeheader()
        w.writerows(rows)

    # Pretty stdout
    print(f"{'event':<28}{'per_hash':>16}{'skeleton':>16}{'skelot/ph':>12}")
    for r in rows:
        ph = r["per_hash"]
        sk = r["skeleton"]
        ratio = r["skelot_div_per_hash"]
        print(
            f"{r['event']:<28}"
            f"{('%.3e' % ph) if ph is not None else 'NA':>16}"
            f"{('%.3e' % sk) if sk is not None else 'NA':>16}"
            f"{('%.4f' % ratio) if ratio is not None else 'NA':>12}"
        )

    print("\nDerived metrics:")
    for k in summary["derived"]["per_hash"]:
        ph = summary["derived"]["per_hash"].get(k)
        sk = summary["derived"]["skeleton"].get(k)
        if ph is None or sk is None:
            continue
        ratio = sk / ph if ph else None
        print(
            f"  {k:<32}  per_hash={ph:>10.4f}  skeleton={sk:>10.4f}  ratio={ratio:>6.4f}"
        )


if __name__ == "__main__":
    main()
