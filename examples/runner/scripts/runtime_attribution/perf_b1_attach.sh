#!/usr/bin/env bash
# Same per-mode bench wrap, but perf attaches via -p AFTER the bench prints
# its MEASUREMENT_PHASE_BEGIN marker on stderr. This gives precise alignment
# of the perf window with Phase 5 only — Phase 1-4 setup is fully excluded.
#
# Output (per RUN_TAG):
#   perf_<mode>_<event_set>.<RUN_TAG>.stat
#   perf_<mode>_<event_set>.<RUN_TAG>.bench.log    bench stdout/stderr
#   perf_<mode>_<event_set>.<RUN_TAG>.perf.log     perf stderr / wall

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

EVENTS_ICACHE="cycles,instructions,L1-icache-load-misses"
EVENTS_OTHER="cycles,instructions,iTLB-load-misses,branch-misses,branches"

RUN_TAG=${RUN_TAG:-r1}

run_one() {
    local mode=$1
    local event_set=$2
    local events=$3
    local stat="$OUT_DIR/perf_${mode}_${event_set}.${RUN_TAG}.stat"
    local bench_log="$OUT_DIR/perf_${mode}_${event_set}.${RUN_TAG}.bench.log"
    local perf_log="$OUT_DIR/perf_${mode}_${event_set}.${RUN_TAG}.perf.log"
    echo "==== mode=$mode event_set=$event_set tag=$RUN_TAG ===="

    # Stream bench output line-by-line. When MEASUREMENT_PHASE_BEGIN appears,
    # spawn perf stat -p $PID. The bench's eprintln line lands on its own stderr,
    # so we redirect stderr to a fifo we read from; stdout still goes to bench_log.
    : > "$bench_log"
    : > "$perf_log"
    : > "$stat"

    local fifo
    fifo=$(mktemp -u /tmp/perf_b1_fifo_XXXXXX)
    mkfifo "$fifo"
    local perf_pid_file
    perf_pid_file=$(mktemp /tmp/perf_b1_pid_XXXXXX)

    # Start bench in background; stdout to bench_log, stderr to fifo
    (
        "$BIN" \
            --bench-dir "$BENCH_DATA" \
            --per-hash-cache "$PH_CACHE" \
            --skeleton-cache "$SK_CACHE" \
            --registry "$REGISTRY" \
            --start-block 38004930 --block-count $BLOCK_COUNT --sample-every $SAMPLE_EVERY \
            --rounds $ROUNDS --warmup $WARMUP \
            --only-mode "$mode" \
            > "$bench_log" 2> "$fifo"
    ) &
    local bench_pid=$!
    echo "bench pid: $bench_pid" | tee -a "$perf_log"

    # Read fifo until we see the marker, mirror to log; then attach perf.
    local marker_seen=0
    {
        while IFS= read -r line; do
            echo "$line" >> "$bench_log"
            if [[ "$marker_seen" -eq 0 && "$line" == *MEASUREMENT_PHASE_BEGIN* ]]; then
                marker_seen=1
                echo "marker seen at $(date +%T.%3N), attaching perf..." >> "$perf_log"
                /usr/bin/time -f "perf wall=%E user=%U sys=%S" \
                    perf stat -x, -o "$stat" -e "$events" -p "$bench_pid" \
                    >> "$perf_log" 2>&1 &
                local perf_pid=$!
                echo "$perf_pid" > "$perf_pid_file"
                echo "perf pid: $perf_pid" >> "$perf_log"
            fi
        done < "$fifo"
    } &
    local reader_pid=$!

    # `|| true` is load-bearing under `set -e`: an unguarded `wait` on a
    # non-zero exit kills the script before rc can be recorded, silently
    # dropping the remaining cells of the grid.
    local bench_rc=0
    wait "$bench_pid" || bench_rc=$?
    echo "bench exited rc=$bench_rc" >> "$perf_log"
    # Reader will EOF when fifo writer (bench) closes; wait for it
    wait "$reader_pid" 2>/dev/null || true
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
    echo "stat written: $stat"
    cat "$stat"
}

cd "$REVMC"
run_one per_hash icache "$EVENTS_ICACHE"
run_one skeleton icache "$EVENTS_ICACHE"
run_one per_hash other "$EVENTS_OTHER"
run_one skeleton other "$EVENTS_OTHER"
echo "ALL_DONE_TAG=$RUN_TAG"
