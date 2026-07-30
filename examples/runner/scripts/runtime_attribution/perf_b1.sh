#!/usr/bin/env bash
# Stage B perf-stat wrap for skeleton vs per_hash dispatch.
#
# Runs the modified skeleton_vs_perhash_bench with --only-mode set to one of
# {per_hash, skeleton}, wrapped with `perf stat -D 35000` so hardware
# counters are enabled only after Phase 1-4 setup completes (~25-30 s wall
# after warm page cache). Setup is identical between both modes; the
# remaining counts are attributable to the measurement loop.
#
# Output:
#   perf_<mode>_<event_set>.log         per-run stderr+stdout
#   perf_summary.json                    parsed counters per mode
#
# Usage:
#   bash perf_b1.sh        runs the full grid (4 perf invocations)
#
set -euo pipefail

REVMC="${REVMC_ROOT:?Set REVMC_ROOT to the revmc checkout root, e.g. $(git rev-parse --show-toplevel)}"
OUT_DIR="${OUT_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/output}"
BIN=$REVMC/target/release/skeleton_vs_perhash_bench
PH_CACHE="${PERHASH_CACHE:?Set PERHASH_CACHE to the per-hash .so cache directory; see README for layout}"
SK_CACHE="${SKELOT_CACHE:?Set SKELOT_CACHE to the skelot .so cache directory; see README}"
REGISTRY="${SKELOT_REGISTRY:?Set SKELOT_REGISTRY to the skeleton_registry_full.bin path}"
BENCH_DATA="${BLOCK_DATA:?Set BLOCK_DATA to the block dataset directory (must contain states/ and txs/)}"

# Workload: 10 sampled blocks, 5 measured rounds + 1 warmup, single mode.
BLOCK_COUNT=1000
SAMPLE_EVERY=100
ROUNDS=30
WARMUP=5
SETUP_DELAY_MS=35000

EVENTS_ICACHE="cycles,instructions,L1-icache-load-misses,L1-icache-loads"
EVENTS_OTHER="cycles,instructions,iTLB-load-misses,branch-misses,branches"

RUN_TAG=${RUN_TAG:-r1}

run_perf() {
    local mode=$1
    local event_set=$2
    local events=$3
    local logfile="$OUT_DIR/perf_${mode}_${event_set}.${RUN_TAG}.log"
    local statfile="$OUT_DIR/perf_${mode}_${event_set}.${RUN_TAG}.stat"
    echo "==== running mode=$mode event_set=$event_set ===="
    /usr/bin/time -f "wall=%E rss=%M user=%U sys=%S" \
    perf stat -D ${SETUP_DELAY_MS} -x, -o "$statfile" \
        -e "$events" -- \
        "$BIN" \
        --bench-dir "$BENCH_DATA" \
        --per-hash-cache "$PH_CACHE" \
        --skeleton-cache "$SK_CACHE" \
        --registry "$REGISTRY" \
        --start-block 38004930 --block-count $BLOCK_COUNT --sample-every $SAMPLE_EVERY \
        --rounds $ROUNDS --warmup $WARMUP \
        --only-mode "$mode" \
        > "$logfile" 2>&1
    echo "wrote $statfile"
    cat "$statfile"
}

cd "$REVMC"

run_perf per_hash icache "$EVENTS_ICACHE"
run_perf skeleton icache "$EVENTS_ICACHE"
run_perf per_hash other "$EVENTS_OTHER"
run_perf skeleton other "$EVENTS_OTHER"

echo "ALL_DONE"
