"""Aggregate per-target-tx perf bracket runs with the frontend and backend
event sets to fully close P1.

For each available event_set in {frontend, backend, tma_l1, tma_l2_bw,
tma_l3_fetch}, sums counters
across r1/r2/r3 within each mode, then for each metric compares per-instruction (or
per-call) skelot/per_hash ratios. The arithmetic question:

  Does Δcycles per call = (sk_cyc/call - ph_cyc/call) match the sum of
  the stall deltas under the same envelope?

The frontend/backend stall event sets are diagnostic and not mutually
exclusive. The tma_l1 event set is the top-level additive slot accounting:
frontend_bound is inferred as slots - retiring - bad_spec - backend_bound.

Outputs:
  perf_p1_summary.{csv,json}
  perf_p1_per_contract_frontend.csv
  perf_p1_per_contract_backend.csv
  perf_p1_per_contract_tma_l1.csv, if the tma_l1 run is present
"""
from __future__ import annotations

import csv
import os
import json
import statistics
from pathlib import Path

import numpy as np

OUT = Path(os.environ.get("OUT_DIR") or Path(__file__).resolve().parent / "output")
OUT.mkdir(parents=True, exist_ok=True)
EVENT_SETS = os.environ.get("EVENT_SETS", "frontend backend tma_l1").split()
MODES = ["per_hash", "skeleton"]
TAGS = os.environ.get("TAGS", "r1 r2 r3").split()


def load(event_set: str, mode: str, tag: str) -> dict:
    p = OUT / f"perf_per_target_{event_set}_{mode}.{tag}.json"
    return json.loads(p.read_text())


def available_tags(event_set: str) -> list[str]:
    return [
        tag
        for tag in TAGS
        if all((OUT / f"perf_per_target_{event_set}_{mode}.{tag}.json").exists() for mode in MODES)
    ]


def sum_counts_per_hash(event_set: str, tags: list[str]) -> dict[str, dict[str, dict[str, int]]]:
    """code_hash -> mode -> {calls, cycles, instructions, e2..e5, wall_ns}."""
    by_hash: dict[str, dict[str, dict[str, int]]] = {}
    for tag in tags:
        for mode in MODES:
            d = load(event_set, mode, tag)
            for row in d[f"{mode}_mode"]["per_hash"]:
                e = by_hash.setdefault(row["code_hash"], {}).setdefault(
                    mode,
                    {
                        "calls": 0,
                        "cycles": 0,
                        "instructions": 0,
                        "e2": 0,
                        "e3": 0,
                        "e4": 0,
                        "e5": 0,
                        "wall_ns": 0,
                    },
                )
                for k in e:
                    e[k] += row[k] if k in row else 0
    return by_hash


def event_order(event_set: str) -> list[str]:
    """Read event_order from any one JSON for this event_set."""
    d = load(event_set, "per_hash", "r1")
    return list(d["per_hash_mode"]["event_order"])


def main():
    summary = {}
    for event_set in EVENT_SETS:
        tags = available_tags(event_set)
        if not tags:
            print(f"=== {event_set} event set ===")
            print("  skipped: missing per-mode JSON files")
            continue
        eo = event_order(event_set)
        per_hash_data = sum_counts_per_hash(event_set, tags)

        # Per-contract rows
        rows = []
        for h, modes in per_hash_data.items():
            if "per_hash" not in modes or "skeleton" not in modes:
                continue
            ph = modes["per_hash"]
            sk = modes["skeleton"]
            if ph["calls"] == 0 or sk["calls"] == 0:
                continue
            row = {
                "code_hash": h,
                "ph_calls": ph["calls"],
                "sk_calls": sk["calls"],
                "ph_cyc_per_call": ph["cycles"] / ph["calls"],
                "sk_cyc_per_call": sk["cycles"] / sk["calls"],
                "cyc_per_call_delta_sk_minus_ph": sk["cycles"] / sk["calls"]
                - ph["cycles"] / ph["calls"],
                "cyc_per_call_ratio_sk_over_ph": (sk["cycles"] / sk["calls"])
                / (ph["cycles"] / ph["calls"]),
            }
            for i, name in enumerate(eo[2:], start=2):
                row[f"ph_{name}_per_call"] = ph[f"e{i}"] / ph["calls"]
                row[f"sk_{name}_per_call"] = sk[f"e{i}"] / sk["calls"]
                row[f"{name}_per_call_delta_sk_minus_ph"] = (
                    sk[f"e{i}"] / sk["calls"] - ph[f"e{i}"] / ph["calls"]
                )
            rows.append(row)

        # Per-contract CSV
        with open(OUT / f"perf_p1_per_contract_{event_set}.csv", "w", newline="") as f:
            w = csv.DictWriter(f, fieldnames=list(rows[0].keys()))
            w.writeheader()
            w.writerows(rows)

        # Globals over all contracts
        ph_tot = {
            k: sum(per_hash_data[h]["per_hash"][k] for h in per_hash_data if "per_hash" in per_hash_data[h])
            for k in ["calls", "cycles", "instructions", "e2", "e3", "e4", "e5", "wall_ns"]
        }
        sk_tot = {
            k: sum(
                per_hash_data[h]["skeleton"][k]
                for h in per_hash_data
                if "skeleton" in per_hash_data[h]
            )
            for k in ["calls", "cycles", "instructions", "e2", "e3", "e4", "e5", "wall_ns"]
        }

        # Per-call deltas at the global level
        global_block = {
            "event_order": eo,
            "tags": tags,
            "n_runs": len(tags),
            "n_contracts_in_both_modes": len(rows),
            "per_hash_totals": ph_tot,
            "skeleton_totals": sk_tot,
            "global_per_call": {
                "ph_cyc": ph_tot["cycles"] / ph_tot["calls"],
                "sk_cyc": sk_tot["cycles"] / sk_tot["calls"],
                "delta_cyc_sk_minus_ph": sk_tot["cycles"] / sk_tot["calls"]
                - ph_tot["cycles"] / ph_tot["calls"],
                "ratio_cyc_sk_over_ph": (sk_tot["cycles"] / sk_tot["calls"])
                / (ph_tot["cycles"] / ph_tot["calls"]),
            },
        }
        for i, name in enumerate(eo[2:], start=2):
            global_block["global_per_call"][f"ph_{name}"] = ph_tot[f"e{i}"] / ph_tot["calls"]
            global_block["global_per_call"][f"sk_{name}"] = sk_tot[f"e{i}"] / sk_tot["calls"]
            global_block["global_per_call"][f"delta_{name}_sk_minus_ph"] = (
                sk_tot[f"e{i}"] / sk_tot["calls"]
                - ph_tot[f"e{i}"] / ph_tot["calls"]
            )

        # Account: how much of Δcycles is explained by stall deltas?
        delta_cyc = global_block["global_per_call"]["delta_cyc_sk_minus_ph"]
        if event_set == "frontend":
            # eo[2..6] = icache_data_stalls, icache_tag_stalls,
            # dsb2mite_penalty_cycles, cycle_activity_stalls_total
            d_icd = global_block["global_per_call"]["delta_icache_data_stalls_sk_minus_ph"]
            d_ict = global_block["global_per_call"]["delta_icache_tag_stalls_sk_minus_ph"]
            d_dsb = global_block["global_per_call"]["delta_dsb2mite_penalty_cycles_sk_minus_ph"]
            d_total = global_block["global_per_call"][
                "delta_cycle_activity_stalls_total_sk_minus_ph"
            ]
            global_block["accounting"] = {
                "delta_cyc": delta_cyc,
                "delta_icache_data_stalls": d_icd,
                "delta_icache_tag_stalls": d_ict,
                "delta_dsb2mite_penalty_cycles": d_dsb,
                "delta_stalls_total_backend": d_total,
                # DSB->MITE switch penalty cycles are frontend stalls too:
                # they add into the frontend attribution, not subtract.
                "delta_frontend_attributed": d_icd + d_ict + d_dsb,
                "frontend_share_of_cyc_delta": (d_icd + d_ict + d_dsb) / delta_cyc
                if delta_cyc != 0
                else None,
                "interpretation": (
                    "If skelot reduces cycles (delta_cyc < 0), and the "
                    "icache stall reductions are themselves negative "
                    "(skelot has fewer stalls), then frontend_share captures "
                    "what % of the cycle savings is in icache_data + icache_tag "
                    "stalls (minus the dsb2mite penalty if it grew)."
                ),
            }
        elif event_set == "backend":
            d_total = global_block["global_per_call"][
                "delta_cycle_activity_stalls_total_sk_minus_ph"
            ]
            d_l1d = global_block["global_per_call"][
                "delta_cycle_activity_stalls_l1d_miss_sk_minus_ph"
            ]
            d_dsb_uops = global_block["global_per_call"]["delta_idq_dsb_uops_sk_minus_ph"]
            d_mite_uops = global_block["global_per_call"]["delta_idq_mite_uops_sk_minus_ph"]
            ph_dsb = global_block["global_per_call"]["ph_idq_dsb_uops"]
            ph_mite = global_block["global_per_call"]["ph_idq_mite_uops"]
            sk_dsb = global_block["global_per_call"]["sk_idq_dsb_uops"]
            sk_mite = global_block["global_per_call"]["sk_idq_mite_uops"]
            global_block["accounting"] = {
                "delta_cyc": delta_cyc,
                "delta_stalls_total": d_total,
                "delta_stalls_l1d_miss": d_l1d,
                "delta_idq_dsb_uops": d_dsb_uops,
                "delta_idq_mite_uops": d_mite_uops,
                "ph_dsb_hit_rate_pct": 100 * ph_dsb / (ph_dsb + ph_mite)
                if (ph_dsb + ph_mite) > 0
                else None,
                "sk_dsb_hit_rate_pct": 100 * sk_dsb / (sk_dsb + sk_mite)
                if (sk_dsb + sk_mite) > 0
                else None,
                "backend_stall_share_of_cyc_delta": d_total / delta_cyc
                if delta_cyc != 0
                else None,
            }
        elif event_set == "tma_l1":
            ph_slots = global_block["global_per_call"]["ph_topdown_slots"]
            sk_slots = global_block["global_per_call"]["sk_topdown_slots"]
            ph_retiring = global_block["global_per_call"]["ph_uops_retired_slots"]
            sk_retiring = global_block["global_per_call"]["sk_uops_retired_slots"]
            ph_bad_spec = global_block["global_per_call"]["ph_topdown_bad_spec_slots"]
            sk_bad_spec = global_block["global_per_call"]["sk_topdown_bad_spec_slots"]
            ph_backend = global_block["global_per_call"]["ph_topdown_backend_bound_slots"]
            sk_backend = global_block["global_per_call"]["sk_topdown_backend_bound_slots"]

            ph_frontend = ph_slots - ph_retiring - ph_bad_spec - ph_backend
            sk_frontend = sk_slots - sk_retiring - sk_bad_spec - sk_backend
            d_slots = sk_slots - ph_slots
            d_retiring = sk_retiring - ph_retiring
            d_bad_spec = sk_bad_spec - ph_bad_spec
            d_backend = sk_backend - ph_backend
            d_frontend = sk_frontend - ph_frontend
            d_sum = d_retiring + d_bad_spec + d_backend + d_frontend
            global_block["global_per_call"].update(
                {
                    "ph_topdown_frontend_bound_slots": ph_frontend,
                    "sk_topdown_frontend_bound_slots": sk_frontend,
                    "delta_topdown_frontend_bound_slots_sk_minus_ph": d_frontend,
                }
            )
            global_block["accounting"] = {
                "delta_cyc": delta_cyc,
                "delta_topdown_slots": d_slots,
                "delta_retiring_slots": d_retiring,
                "delta_bad_spec_slots": d_bad_spec,
                "delta_backend_bound_slots": d_backend,
                "delta_frontend_bound_slots": d_frontend,
                "delta_sum_retiring_bad_spec_backend_frontend": d_sum,
                "additive_residual_slots": d_slots - d_sum,
                "frontend_bound_share_of_slot_delta": d_frontend / d_slots
                if d_slots != 0
                else None,
                "backend_bound_share_of_slot_delta": d_backend / d_slots
                if d_slots != 0
                else None,
                "interpretation": (
                    "Top-level TMA slot accounting is additive: frontend_bound "
                    "is inferred as slots - retiring - bad_spec - backend_bound. "
                    "This is orthogonal slot accounting, not a causal decomposition "
                    "of all code-generation mechanisms."
                ),
            }
        elif event_set == "tma_l2_bw":
            ph_dsb_any = global_block["global_per_call"]["ph_idq_dsb_cycles_any"]
            sk_dsb_any = global_block["global_per_call"]["sk_idq_dsb_cycles_any"]
            ph_dsb_ok = global_block["global_per_call"]["ph_idq_dsb_cycles_ok"]
            sk_dsb_ok = global_block["global_per_call"]["sk_idq_dsb_cycles_ok"]
            ph_mite_any = global_block["global_per_call"]["ph_idq_mite_cycles_any"]
            sk_mite_any = global_block["global_per_call"]["sk_idq_mite_cycles_any"]
            ph_mite_ok = global_block["global_per_call"]["ph_idq_mite_cycles_ok"]
            sk_mite_ok = global_block["global_per_call"]["sk_idq_mite_cycles_ok"]

            ph_dsb_limited = max(0.0, ph_dsb_any - ph_dsb_ok)
            sk_dsb_limited = max(0.0, sk_dsb_any - sk_dsb_ok)
            ph_mite_limited = max(0.0, ph_mite_any - ph_mite_ok)
            sk_mite_limited = max(0.0, sk_mite_any - sk_mite_ok)
            d_dsb_limited = sk_dsb_limited - ph_dsb_limited
            d_mite_limited = sk_mite_limited - ph_mite_limited

            global_block["global_per_call"].update(
                {
                    "ph_idq_dsb_limited_cycles": ph_dsb_limited,
                    "sk_idq_dsb_limited_cycles": sk_dsb_limited,
                    "delta_idq_dsb_limited_cycles_sk_minus_ph": d_dsb_limited,
                    "ph_idq_mite_limited_cycles": ph_mite_limited,
                    "sk_idq_mite_limited_cycles": sk_mite_limited,
                    "delta_idq_mite_limited_cycles_sk_minus_ph": d_mite_limited,
                }
            )
            global_block["accounting"] = {
                "delta_cyc": delta_cyc,
                "delta_idq_dsb_cycles_any": sk_dsb_any - ph_dsb_any,
                "delta_idq_dsb_cycles_ok": sk_dsb_ok - ph_dsb_ok,
                "delta_idq_mite_cycles_any": sk_mite_any - ph_mite_any,
                "delta_idq_mite_cycles_ok": sk_mite_ok - ph_mite_ok,
                "delta_idq_dsb_limited_cycles": d_dsb_limited,
                "delta_idq_mite_limited_cycles": d_mite_limited,
                "delta_idq_limited_cycles_total": d_dsb_limited + d_mite_limited,
                "interpretation": (
                    "TMA L2 frontend-bandwidth diagnostics: DSB/MITE limited "
                    "cycles are derived from IDQ *_CYCLES_ANY - *_CYCLES_OK. "
                    "These are mechanism proxies, not additive children of the "
                    "TMA-L1 frontend-bound slot delta."
                ),
            }
        elif event_set == "tma_l3_fetch":
            d_icd = global_block["global_per_call"]["delta_icache_data_stalls_sk_minus_ph"]
            d_ict = global_block["global_per_call"]["delta_icache_tag_stalls_sk_minus_ph"]
            d_offcore = global_block["global_per_call"][
                "delta_offcore_cycles_demand_code_rd_sk_minus_ph"
            ]
            d_l2_miss = global_block["global_per_call"][
                "delta_l2_rqsts_code_rd_miss_sk_minus_ph"
            ]
            global_block["accounting"] = {
                "delta_cyc": delta_cyc,
                "delta_icache_data_stalls": d_icd,
                "delta_icache_tag_stalls": d_ict,
                "delta_offcore_cycles_demand_code_rd": d_offcore,
                "delta_l2_rqsts_code_rd_miss": d_l2_miss,
                "interpretation": (
                    "TMA L3/L4 frontend-latency diagnostics around instruction "
                    "fetch. icache_data/tag are the L1i stall components used by "
                    "tma_icache_misses; offcore_cycles_demand_code_rd is the "
                    "tma_code_l2_miss numerator; l2_rqsts_code_rd_miss counts "
                    "L2 code-read misses. These counters are not mutually "
                    "exclusive additive buckets."
                ),
            }

        summary[event_set] = global_block

    (OUT / "perf_p1_summary.json").write_text(json.dumps(summary, indent=2))

    # Pretty print
    for event_set in summary:
        b = summary[event_set]
        print(f"=== {event_set} event set ===")
        print(f"  n_contracts_in_both_modes: {b['n_contracts_in_both_modes']}")
        print(f"  event_order: {b['event_order']}")
        gp = b["global_per_call"]
        print(f"  per-call cycles: per_hash={gp['ph_cyc']:.0f}  skeleton={gp['sk_cyc']:.0f}")
        print(
            f"    delta_cyc (sk - ph) = {gp['delta_cyc_sk_minus_ph']:.0f}  "
            f"ratio = {gp['ratio_cyc_sk_over_ph']:.4f}"
        )
        for i, name in enumerate(b["event_order"][2:], start=2):
            ph_v = gp.get(f"ph_{name}")
            sk_v = gp.get(f"sk_{name}")
            d_v = gp.get(f"delta_{name}_sk_minus_ph")
            if ph_v is not None and sk_v is not None and d_v is not None:
                print(
                    f"  per-call {name}: per_hash={ph_v:.1f}  skeleton={sk_v:.1f}  "
                    f"delta = {d_v:+.1f}"
                )
        if "accounting" in b:
            print("  accounting:")
            for k, v in b["accounting"].items():
                if isinstance(v, str):
                    continue
                print(f"    {k}: {v}")


if __name__ == "__main__":
    main()
