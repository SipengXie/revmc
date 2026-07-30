#!/usr/bin/env bash
# P3 closure: cross-instance B (data-table cost) measurement.
#
# For the 85 families covered by stage_e_so_cache, compare two skeleton
# caches in the cross-instance bench:
#   - auto:   skelot_auto.so (most PUSHes inlined as movq-imm)
#   - forced: skelot_forced.so (every PUSH via data-table load)
#
# Both pass dispatches identical workloads. The cycle/call delta between
# them, restricted to the 85 covered families' target contracts, is the
# data-table cost B in the cross-instance regime.
#
# Outputs:
#   perf_p3_{auto,forced}.r{1,2,3}.json + corresponding .log
#
# Note: the bench reports families NOT covered by these caches as warnings
# at load time and falls back to per_hash dispatch for them. The
# `auto` and `forced` runs have IDENTICAL coverage (same 85 families
# represented), so non-covered families contribute equally to both runs
# and cancel in the per-contract comparison.

set -euo pipefail

REVMC="${REVMC_ROOT:?Set REVMC_ROOT to the revmc checkout root, e.g. $(git rev-parse --show-toplevel)}"
OUT_DIR="${OUT_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/output}"
BIN=$REVMC/target/release/skeleton_vs_perhash_bench
PH_CACHE="${PERHASH_CACHE:?Set PERHASH_CACHE to the per-hash .so cache directory; see README for layout}"
BENCH_DATA="${BLOCK_DATA:?Set BLOCK_DATA to the block dataset directory (must contain states/ and txs/)}"
AUTO_CACHE=/tmp/skel_85_auto
FORCED_CACHE=/tmp/skel_85_forced

BLOCK_COUNT=1000
SAMPLE_EVERY=100
ROUNDS=5
WARMUP=1

run_one() {
    local variant=$1
    local cache=$2
    local classify=$3
    local tag=$4
    local out="$OUT_DIR/perf_p3_${variant}.${tag}.json"
    local log="$OUT_DIR/perf_p3_${variant}.${tag}.log"
    echo "==== variant=$variant classify=$classify tag=$tag ===="
    /usr/bin/time -f "wall=%E rss=%M" \
        "$BIN" \
        --bench-dir "$BENCH_DATA" \
        --per-hash-cache "$PH_CACHE" \
        --skeleton-cache "$cache" \
        --start-block 38004930 --block-count $BLOCK_COUNT --sample-every $SAMPLE_EVERY \
        --rounds $ROUNDS --warmup $WARMUP \
        --only-mode skeleton \
        --classify-mode "$classify" \
        --perf-out "$out" \
        > "$log" 2>&1
}

cd "$REVMC"
for tag in r1 r2 r3; do
    run_one auto "$AUTO_CACHE" auto "$tag"
    run_one forced "$FORCED_CACHE" force-all-variant "$tag"
done
echo ALL_DONE
