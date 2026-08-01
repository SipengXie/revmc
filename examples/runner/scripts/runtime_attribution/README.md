# Reproducing the SkelOT runtime attribution analyses

This directory contains the scripts and parsers that reproduce every
quantitative claim in the runtime-attribution writeup for the SkelOT
project (a per-family AOT compilation strategy for the EVM, built on
top of `skeleton_vs_perhash_bench`).

The scripts are split into:

* **bench drivers** (`*.sh`) — invoke
  `target/release/skeleton_vs_perhash_bench` with the right CLI flags
  and write per-target-tx perf-event counters as JSON;
* **analysis** (`*.py`) — pure Python, read either the JSON dumps from
  the bench or read-only paper inputs (`skeleton_vs_perhash.json`,
  `compile_bench.json`, etc.) and produce the tables, figures, and
  summary statistics referenced by the report.

## 0. One-time setup

### Build the bench

```bash
cd "$REVMC_ROOT"  # the repo root containing this directory
cargo build --release -p revmc-examples-runner --bin skeleton_vs_perhash_bench
```

### Configure paths

```bash
cd examples/runner/scripts/runtime_attribution
cp paths.env.example paths.env
$EDITOR paths.env       # fill in BLOCK_DATA, PERHASH_CACHE, SKELOT_CACHE, ...
set -a; source paths.env; set +a
```

`paths.env` is gitignored, so the file with your machine's absolute
paths never lands in the public repo. The variables expected by every
script:

| Variable | What it points at |
|----------|-------------------|
| `REVMC_ROOT` | this repo's checkout root (auto-discovers from the script directory if unset) |
| `BLOCK_DATA` | block dataset directory containing `states/<n>.bin` and `txs/<n>.bin` |
| `PERHASH_CACHE` | directory of `<code_hash>__prague__o3.{so,o}` per-bytecode artifacts |
| `SKELOT_CACHE` | directory of `skel_<short>__prague__o3.{so,o}` per-family artifacts (plus per-member files for the family representative) |
| `SKELOT_REGISTRY` | `skeleton_registry_full.bin` — pre-analyzed skeleton registry; auto-built on first bench run if absent |
| `INPUT_DATA_DIR` | directory of read-only paper-input JSONs (see below) |
| `OUT_DIR` | optional; defaults to `output/` next to the scripts |

The `INPUT_DATA_DIR` should contain at least these read-only files,
each of which is the output of an earlier benchmark run on this same
bench harness (see `data/provenance.md` in the paper artifact for the
commands that produced them):

| File | Producer | Used by |
|------|----------|---------|
| `skeleton_vs_perhash.json` | `skeleton_vs_perhash_bench` (full corpus, mixed-mode) | §1, §9 below |
| `compile_bench.json` | `skelot_compile_bench` | §1 below |
| `dataset_scan.json` | `skelot_dataset_scan` | §1 below |
| `skeleton_families.json` | `skeleton_export` | §1 below |
| `ablation_allvariant.families.jsonl` | `skelot_ablation_bench` | §1, §9 below |
| `skeleton_mapping.tsv` | `skeleton_identify` | §1, §2 below |

### System requirements for the perf experiments

* `kernel.perf_event_paranoid <= 0` (these scripts assume `-1`; set
  with `sysctl -w kernel.perf_event_paranoid=-1` if needed).
* The `frontend` and `backend` raw event configs in
  `skeleton_vs_perhash_bench.rs::PerfBracket::new` are
  Sapphire Rapids–specific (Intel Xeon Platinum 8488C). On a different
  CPU, regenerate the configs from `perf -vv` and update the source.

## 1. Stage A — family / artifact / variant-PUSH joins

Pure re-analysis of the read-only JSON inputs. No bench.

```bash
python3 analysis_a1.py
```

Produces `joined_per_contract.csv` (one row per target contract,
joined across `skeleton_vs_perhash.json`, `compile_bench.json`,
`skeleton_families.json`, and `ablation_allvariant.families.jsonl`),
`signals.json` (Spearman correlations and partial correlations across
`family_size`, `skeleton_len`, `naive_o_bytes`, `o_bytes_ratio`,
`av_variant_ratio`), bucketed median tables, and scatter plots.

## 2. Stage B — per-instance .text size comparison

Reads pre-compiled `.so` files and runs `size` on them.

```bash
python3 disasm_b2.py
```

Picks the largest skeleton family and quantifies SkelOT's
per-instance `.text` overhead vs the per-bytecode artifacts in
`PERHASH_CACHE`.

## 3. Stage B — process-level perf attach

Two `perf stat` invocations per mode, attached via `-p $PID`
immediately after the bench prints `MEASUREMENT_PHASE_BEGIN` so that
Phase 1–4 setup is excluded from counters. Three runs for stability.

```bash
for tag in attach1 attach2 attach3; do
    RUN_TAG=$tag bash perf_b1_attach.sh
done
python3 parse_perf_attach.py
```

Produces `perf_*_*.attach{1,2,3}.stat` plus
`perf_attach_summary.{csv,json}` with three-run mean / stddev for
cycles, instructions, L1i miss, iTLB miss, branches, branch-misses
under both `--only-mode per_hash` and `--only-mode skeleton`.

## 4. Stage B — per-target-tx 6-event bracket

`PerfBracket::new(PerfEventSet::Default)` opens a 6-event group
(`cycles`, `instructions`, `L1i miss`, `iTLB miss`, `branches`,
`branch-misses`) at the start of Phase 5. Every JIT call on a target
`bytecode_hash` is bracketed with two `read()` syscalls; per-hash
counters are dumped via `--perf-out`.

```bash
for tag in r1 r2 r3; do
    RUN_TAG=$tag bash perf_per_target_run.sh
done
python3 parse_perf_per_target.py
```

Produces `perf_per_target_*.r{1,2,3}.json` plus
`perf_per_target_summary.{csv,json}` and
`perf_per_target_per_contract.csv`.

## 5. Stage B — per-target-tx with wall_ns (P2 follow-up)

The same per-target experiment as §4, but the JSON now also carries
per-call `wall_ns` (`Instant::elapsed()` inside the same bracket) so
in-bracket cycle and wall ratios can be compared per contract.

```bash
for tag in wall_r1 wall_r2 wall_r3; do
    RUN_TAG=$tag bash perf_per_target_run.sh
done
python3 parse_perf_p2_inbracket.py
```

Produces `perf_per_target_*.wall_r{1,2,3}.json`,
`perf_p2_inbracket_summary.{csv,json}`, and
`perf_p2_inbracket_per_contract.csv`.

## 6. Stage B — per-target stall bracket plus TMA-L1 slots (P1 closure)

Three raw-event groups cover two different questions. `PerfEventSet::Frontend`
and `Backend` collect diagnostic counters for icache fetch stalls, total
execution stalls, L1d-miss-induced stalls, DSB↔MITE switch penalty, and
DSB / MITE uop counts. These counters are not mutually exclusive accounting
buckets. `PerfEventSet::TmaL1` collects top-level TMA slots and infers
frontend-bound slots by subtraction from slots, retiring slots,
bad-speculation slots, and backend-bound slots. Three rounds × two modes ×
three event groups = 18 invocations.

```bash
bash perf_p1_run.sh
python3 parse_perf_p1.py
```

If the frontend/backend JSONs already exist and only the additive slot
accounting is missing, run just the new event set:

```bash
EVENT_SETS=tma_l1 bash perf_p1_run.sh
python3 parse_perf_p1.py
```

Produces `perf_per_target_{frontend,backend,tma_l1}_*.r{1,2,3}.json`,
`perf_p1_summary.json`, and
`perf_p1_per_contract_{frontend,backend,tma_l1}.csv`.

The parser reports the frontend/backend stall counters as an evidence stack,
not an additive pie. The additive top-level decomposition comes only from
the TMA-L1 slot set:
`slots = retiring + bad_speculation + backend_bound + frontend_bound`, with
`frontend_bound` inferred by subtraction.

## 7. Stage B — process-level residual stalls (legacy)

The first attempt at the stall-decomposition question, attached at
process granularity rather than per-target. Superseded by §6 but still
useful as an independent, lower-resolution sanity check.

```bash
RUN_TAG=r1 bash perf_residual_attach.sh
```

Produces `perf_residual_*_{frontend,backend,dsb}.r1.{stat,perf.log,bench.log}`.

## 8. Stage B — cross-instance data-table cost (P3 closure)

Compares `skelot_auto.so` vs `skelot_forced.so` (the latter forces
every PUSH through the per-member data table) on the same workload.
Both variants run on a restricted 85-family subset for which
both `.so` files are available.

First time only — build the symlink trees that the bench's
`--skeleton-cache` will read from:

```bash
mkdir -p /tmp/skel_85_auto /tmp/skel_85_forced
ls "$STAGE_E_CACHE"/cap16 "$STAGE_E_CACHE"/cap32 | sort -u > /tmp/stage_e_fams_uniq.txt
while read fam; do
    bench_auto="$SKELOT_CACHE/skel_${fam}__prague__o3.so"
    se_forced16="$STAGE_E_CACHE/cap16/$fam/skelot_forced.so"
    se_forced32="$STAGE_E_CACHE/cap32/$fam/skelot_forced.so"
    [ -f "$bench_auto" ] && ln -sf "$bench_auto" /tmp/skel_85_auto/skel_${fam}__prague__o3.so
    if [ -f "$se_forced32" ]; then
        ln -sf "$se_forced32" /tmp/skel_85_forced/skel_${fam}__prague__o3.so
    elif [ -f "$se_forced16" ]; then
        ln -sf "$se_forced16" /tmp/skel_85_forced/skel_${fam}__prague__o3.so
    fi
done < /tmp/stage_e_fams_uniq.txt
```

Set `STAGE_E_CACHE` to point at the per-family `skelot_forced.so` cache
produced by `skelot_ablation_bench`. Then:

```bash
bash perf_p3_run.sh
python3 parse_perf_p3.py
```

Produces `perf_p3_{auto,forced}.r{1,2,3}.json`,
`perf_p3_summary.json`, and `perf_p3_per_contract.csv`. The bench's
`load_skeleton_so` falls back to the `skelot_auto` / `skelot_forced`
symbols when the expected `skel_<short>` symbol is absent, so the
renamed `.so` files in the symlink trees load cleanly.

## 9. Critique-response analyses (P2 regression, P3 single-call, P4 slow contracts)

Pure joins over `joined_per_contract.csv` (§1),
`perf_per_target_per_contract.csv` (§4), and the read-only ablation
file. No bench.

```bash
python3 critique_response.py
```

Produces:

* `critique_response_p2.csv` + the `regression_wall_on_cyc` block of
  `critique_response_summary.json` — slope of the per-contract
  wall_ratio ~ cycle_ratio regression, used to recover the
  framework-overhead share of per-call wall time (P2).
* `critique_response_p3.json` — single-call ablation B upper bound
  (P3).
* `critique_response_p4.csv` and `problem_4_slower_characterization`
  block — characterization of the contracts where SkelOT is slower
  than per-hash compilation, by family size, bytecode length, and
  variant-PUSH ratio (P4).

## CLI flags added to `skeleton_vs_perhash_bench` for these scripts

These flags all default to behavior that reproduces the original
mixed-mode benchmark used to generate `skeleton_vs_perhash.json`. The
scripts in this directory exercise the non-default values.

| Flag | Default | Non-default purpose |
|------|---------|---------------------|
| `--only-mode {both,per_hash,skeleton}` | `both` | restrict the measurement loop to one dispatch mode for clean per-mode hardware-counter wraps |
| `--perf-out <path>` | unset | enable per-target `perf_event_open` bracket and dump per-hash counters as JSON |
| `--perf-events {default,frontend,backend,tma_l1}` | `default` | swap the 6-event group between the default Hardware/Cache events, Sapphire Rapids raw stall events, and additive TMA-L1 slot events |
| `--classify-mode {auto,force-all-variant}` | `auto` | force every PUSH through the data table (P3 cross-instance B test) |

The bench's `load_skeleton_so` also tries the `skelot_forced` and
`skelot_auto` ELF symbols as fallbacks if the expected `skel_<short>`
symbol is not present, so renamed `skelot_*.so` files (such as those
produced by `skelot_ablation_bench`) can be used in the
`SKELOT_CACHE` directory.

## End-to-end one-liner

```bash
set -a; source paths.env; set +a
cargo build --release -p revmc-examples-runner --bin skeleton_vs_perhash_bench
python3 analysis_a1.py
python3 disasm_b2.py
for tag in attach1 attach2 attach3; do RUN_TAG=$tag bash perf_b1_attach.sh; done
for tag in r1 r2 r3; do RUN_TAG=$tag bash perf_per_target_run.sh; done
for tag in wall_r1 wall_r2 wall_r3; do RUN_TAG=$tag bash perf_per_target_run.sh; done
bash perf_p1_run.sh
bash perf_p3_run.sh   # requires the §8 symlink trees built first
python3 parse_perf_attach.py
python3 parse_perf_per_target.py
python3 parse_perf_p2_inbracket.py
python3 parse_perf_p1.py
python3 parse_perf_p3.py
python3 critique_response.py
```

Wall-clock budget on a single Sapphire Rapids node with warm page
cache: the analysis-only steps run in well under a minute each; the
bench-driving steps add up to roughly 90 minutes total.
