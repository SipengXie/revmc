# Full-Block Ablation Design

## Goal

Generate a JIT whitelist that reflects real full-block performance (including L1i cache pressure), not isolated-snapshot performance.

## Motivation

`ablation_bench` measures each contract in isolation (clone snapshot, run single tx). This misses L1i cache thrashing that occurs when 304 JIT functions compete for 128KB L1i during full-block replay. perf stat shows JIT has 3.4x more L1i misses than native in full-block context. Contracts classified as JIT-GOOD in isolation may not benefit in practice.

## Tool

New binary: `examples/runner/src/bin/fullblock_ablation.rs`

```
fullblock_ablation \
  --block 38004930 \
  --dir /home/ubuntu/sipeng/bench_data \
  --cache-dir /tmp/jit_cache \
  --rounds 15 --warmup 3 --alpha 0.05 \
  --output whitelist.json
```

## Architecture

Reuses `bin_common.rs` infrastructure (BinLoader, JitHandler, NativeHandler, AOT cache, DiscoveryHandler pattern from ablation_bench). Adds full-block replay loop with per-tx timing for each ablation configuration.

## Phases

### Phase 0: Baseline

Run full-block replay R rounds for each mode:
- **Native**: all txs via NativeHandler, record per-tx Duration
- **All-JIT**: all txs via JitHandler (full functions map), record per-tx Duration

Each round rebuilds the EVM from `loader.build_cache_db()` and replays all non-deposit txs sequentially. Every tx is timed with `Instant::now()` (overhead proven negligible by prior experiments).

Output: per-tx median native time, per-tx median JIT time, block-level speedup.

### Phase 1: Discovery

For each non-deposit tx, run once with a DiscoveryHandler that records which bytecode hashes are dispatched to JIT (same pattern as ablation_bench's discovery phase).

Build mapping: `contract_hash -> Vec<tx_index>`.

### Phase 2: Per-Contract Ablation

For each unique contract C:
1. Build ablated HashMap: `all_functions.filter(|h| h != C)`
2. Full-block replay R rounds with ablated JIT, timing every tx
3. For each tx that touches C: collect the R ablated samples
4. Welch's t-test: compare all-JIT samples vs ablated samples for each affected tx
5. Compute delta: positive = JIT helps (removing it makes tx slower), negative = JIT hurts

### Phase 3: Aggregate & Output

For each contract, aggregate deltas across all txs that touch it:
- **avg_delta**: mean of per-tx median deltas
- **Verdict**: Welch's t-test on pooled deltas
  - p < alpha AND delta > 0 → JIT-GOOD
  - p < alpha AND delta < 0 → JIT-BAD
  - p >= alpha → NEUTRAL

Output to stdout: sorted table (hash, tx_count, frame_count, avg_delta, p_value, verdict).

Output to JSON file: `{ block, whitelist: [GOOD hashes], blacklist: [BAD hashes] }`.

## Key Differences from ablation_bench

| | ablation_bench | fullblock_ablation |
|--|---------------|-------------------|
| Measurement mode | Isolated snapshot per tx | Full-block replay |
| L1i pressure | None (1 tx at a time) | Realistic (227 txs) |
| Cost | O(txs × contracts × rounds) | O(contracts × rounds × block_size) |
| Statistical method | Welch's t-test | Welch's t-test (same) |

## Cost Estimate

- ~245 unique contracts × (3 warmup + 15 rounds) × 227 txs ≈ 1M tx executions
- At ~30ms per full-block replay ≈ 245 × 18 × 30ms ≈ ~2.2 minutes for ablation phase
- Plus baseline (~1 minute) and discovery (~1 second)
- Total: ~3-4 minutes

## CLI Flags

| Flag | Default | Description |
|------|---------|-------------|
| `--block` | required | Block number |
| `--dir` | `/home/ubuntu/sipeng/bench_data` | Bench data directory |
| `--cache-dir` | required | AOT cache directory |
| `--rounds` | 15 | Measurement rounds per config |
| `--warmup` | 3 | Untimed warmup runs |
| `--alpha` | 0.05 | Significance level for t-test |
| `--output` | None | Path to write whitelist JSON |

## Reused Components

From `bin_common.rs`:
- `BinLoader`, `build_op_cfg`, `build_op_tx`, `compile_all_contracts_with_cache`
- `JitHandler`, `NativeHandler`
- `should_lookup_jit` (for discovery)

From `ablation_bench.rs` (copy/adapt):
- `DiscoveryHandler` pattern
- Welch's t-test implementation
- Aggregate reporting logic
