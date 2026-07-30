#!/usr/bin/env bash
# Per-target-tx perf bracket with frontend + backend stall event sets plus
# an additive TMA-L1 top-level slot set.
# Each set runs 3 rounds × 2 modes = 6 invocations. Outputs JSON whose
# new schema has cycles, instructions, e2..e5 + event_order array.

set -euo pipefail

REVMC="${REVMC_ROOT:?Set REVMC_ROOT to the revmc checkout root, e.g. $(git rev-parse --show-toplevel)}"
OUT_DIR="${OUT_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/output}"
BIN=$REVMC/target/release/skeleton_vs_perhash_bench
PH_CACHE="${PERHASH_CACHE:?Set PERHASH_CACHE to the per-hash .so cache directory; see README for layout}"
SK_CACHE="${SKELOT_CACHE:?Set SKELOT_CACHE to the skelot .so cache directory; see README}"
REGISTRY="${SKELOT_REGISTRY:?Set SKELOT_REGISTRY to the skeleton_registry_full.bin path}"
BENCH_DATA="${BLOCK_DATA:?Set BLOCK_DATA to the block dataset directory (must contain states/ and txs/)}"

BLOCK_COUNT=1000
SAMPLE_EVERY=100
ROUNDS=5
WARMUP=1
TAGS="${TAGS:-r1 r2 r3}"
EVENT_SETS="${EVENT_SETS:-frontend backend tma_l1}"

run_one() {
    local mode=$1
    local event_set=$2
    local tag=$3
    local out="$OUT_DIR/perf_per_target_${event_set}_${mode}.${tag}.json"
    local log="$OUT_DIR/perf_per_target_${event_set}_${mode}.${tag}.log"
    echo "==== mode=$mode event_set=$event_set tag=$tag ===="
    /usr/bin/time -f "wall=%E rss=%M" \
        "$BIN" \
        --bench-dir "$BENCH_DATA" \
        --per-hash-cache "$PH_CACHE" \
        --skeleton-cache "$SK_CACHE" \
        --registry "$REGISTRY" \
        --start-block 38004930 --block-count $BLOCK_COUNT --sample-every $SAMPLE_EVERY \
        --rounds $ROUNDS --warmup $WARMUP \
        --only-mode "$mode" \
        --perf-out "$out" \
        --perf-events "$event_set" \
        > "$log" 2>&1
}

cd "$REVMC"
for tag in $TAGS; do
    for set in $EVENT_SETS; do
        run_one per_hash "$set" "$tag"
        run_one skeleton "$set" "$tag"
    done
done
echo ALL_DONE
