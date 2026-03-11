#!/usr/bin/env bash
set -euo pipefail

LOG="/tmp/skeleton_precompile.log"

echo "Building release binary..."
cargo build -p revmc-examples-runner --bin skeleton_precompile --release 2>&1 | tail -1

echo "Starting skeleton_precompile (log: $LOG)"
echo "  Estimated: ~9.5 hours for 10K blocks"
echo "  Resume-safe: re-run this script to continue from where it left off"
echo ""

nohup cargo run -p revmc-examples-runner --bin skeleton_precompile --release -- \
  --cache-dir /tmp/jit_cache --count 9997 \
  >> "$LOG" 2>&1 &

PID=$!
echo "PID: $PID"
echo "Monitor: tail -f $LOG"
