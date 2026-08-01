#!/usr/bin/env bash
# Frontend/backend stall decomposition to address Problem 1: where do the
# unaccounted-for cycles in the cycle delta come from?
#
# The Sapphire Rapids PMU on this machine doesn't expose modern topdown-retiring
# fixed-counter events, so we use the underlying stall events directly:
#   icache_data.stalls            frontend stall waiting for L1i data
#   icache_tag.stalls             frontend stall waiting for L1i tag
#   dsb2mite_switches.penalty_cycles  DSB (uop cache) → legacy decode penalty
#   cycle_activity.stalls_total   total backend stalls
#   cycle_activity.stalls_l1d_miss backend stall due to L1d miss
#   idq.dsb_uops                  uops delivered from DSB (uop cache hit)
#   idq.mite_uops                 uops delivered from MITE (uop cache miss)
#
# Same attach-on-MEASUREMENT_PHASE_BEGIN pattern as perf_b1_attach.sh.

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
ROUNDS=30
WARMUP=5
RUN_TAG=${RUN_TAG:-r1}

EVENTS_FRONTEND="cycles,instructions,icache_data.stalls,icache_tag.stalls,dsb2mite_switches.penalty_cycles"
EVENTS_BACKEND="cycles,instructions,cycle_activity.stalls_total,cycle_activity.stalls_l1d_miss"
EVENTS_DSB="cycles,instructions,idq.dsb_uops,idq.mite_uops"

run_one() {
    local mode=$1
    local event_set=$2
    local events=$3
    local stat="$OUT_DIR/perf_residual_${mode}_${event_set}.${RUN_TAG}.stat"
    local bench_log="$OUT_DIR/perf_residual_${mode}_${event_set}.${RUN_TAG}.bench.log"
    local perf_log="$OUT_DIR/perf_residual_${mode}_${event_set}.${RUN_TAG}.perf.log"
    : > "$bench_log"; : > "$perf_log"; : > "$stat"
    local fifo perf_pid_file
    fifo=$(mktemp -u /tmp/perf_residual_fifo_XXXXXX)
    mkfifo "$fifo"
    perf_pid_file=$(mktemp /tmp/perf_residual_pid_XXXXXX)
    (
        "$BIN" --bench-dir "$BENCH_DATA" \
            --per-hash-cache "$PH_CACHE" --skeleton-cache "$SK_CACHE" \
            --registry "$REGISTRY" \
            --start-block 38004930 --block-count $BLOCK_COUNT --sample-every $SAMPLE_EVERY \
            --rounds $ROUNDS --warmup $WARMUP --only-mode "$mode" \
            > "$bench_log" 2> "$fifo"
    ) &
    local bench_pid=$!
    {
        while IFS= read -r line; do
            echo "$line" >> "$bench_log"
            if [[ "$line" == *MEASUREMENT_PHASE_BEGIN* ]]; then
                /usr/bin/time -f "perf wall=%E" \
                    perf stat -x, -o "$stat" -e "$events" -p "$bench_pid" \
                    >> "$perf_log" 2>&1 &
                echo $! > "$perf_pid_file"
                break
            fi
        done < "$fifo"
        cat "$fifo" >> "$bench_log" || true
    } &
    local reader=$!
    wait "$bench_pid" || true
    wait "$reader" 2>/dev/null || true
    # perf is a grandchild (spawned inside the reader subshell) so `wait`
    # can't reach it; poll until it has exited and flushed the stat file.
    if [[ -s "$perf_pid_file" ]]; then
        local perf_pid
        perf_pid=$(cat "$perf_pid_file")
        while kill -0 "$perf_pid" 2>/dev/null; do
            sleep 0.1
        done
    fi
    rm -f "$fifo" "$perf_pid_file"
    echo "wrote $stat"
    cat "$stat"
}

cd "$REVMC"
run_one per_hash frontend "$EVENTS_FRONTEND"
run_one skeleton frontend "$EVENTS_FRONTEND"
run_one per_hash backend "$EVENTS_BACKEND"
run_one skeleton backend "$EVENTS_BACKEND"
run_one per_hash dsb "$EVENTS_DSB"
run_one skeleton dsb "$EVENTS_DSB"
echo "ALL_DONE_TAG=$RUN_TAG"
