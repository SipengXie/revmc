#!/usr/bin/env bash
# Per-target-tx hardware counter runs.
#
# Drives the modified skeleton_vs_perhash_bench with --perf-out, which uses
# perf_event_open (via the perf-event crate) to bracket every JIT call on a
# target bytecode_hash with ioctl reads of cycles, instructions,
# L1-icache-miss, iTLB-miss, branches, branch-misses. The bracket is the
# minimal envelope around the JIT call itself — exactly the same logical
# work that the existing skeleton_vs_perhash.json speedup_ratio is computed
# over.
#
# Each invocation runs ONE --only-mode pass; we run per_hash and skeleton
# separately to avoid sharing JIT code-cache state between modes.
#
# Output:
#   perf_per_target_<mode>.<RUN_TAG>.json   per-mode aggregated counters

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

RUN_TAG=${RUN_TAG:-r1}

run_one() {
    local mode=$1
    local out="$OUT_DIR/perf_per_target_${mode}.${RUN_TAG}.json"
    local log="$OUT_DIR/perf_per_target_${mode}.${RUN_TAG}.log"
    echo "==== mode=$mode tag=$RUN_TAG ===="
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
        > "$log" 2>&1
    echo "wrote $out"
}

cd "$REVMC"
run_one per_hash
run_one skeleton
echo "ALL_DONE_TAG=$RUN_TAG"
