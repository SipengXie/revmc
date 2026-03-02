# JIT Ablation Experiment Design

## Goal

Identify which contracts benefit from JIT compilation ("JIT-GOOD") and which are hurt by it ("JIT-BAD") through systematic per-tx ablation testing with statistical rigor.

## Tool

New binary: `examples/runner/src/bin/ablation_bench.rs`

```
ablation_bench --block 38004930 --cache-dir /tmp/jit_cache \
  --rounds 15 --alpha 0.05 --warmup 3
```

## Architecture

Reuses `bin_common.rs` infrastructure (BinLoader, AOT cache, JitHandler, NativeHandler). Adds per-tx snapshot collection, ablation loop, and Welch's t-test.

## Phases

### Phase 0: Per-TX Snapshot Collection

Run block once sequentially, clone `CacheDB` before each tx.

```rust
let mut evm = make_evm(&loader, chain_id);
let mut snapshots: Vec<(usize, CacheDB<EmptyDB>)> = Vec::new();

for (i, tx) in loader.raw_txs().iter().enumerate() {
    let snapshot_db = evm.ctx().db().clone();
    snapshots.push((i, snapshot_db));
    evm.ctx_mut().set_tx(build_op_tx(tx));
    evm.transact_commit()?;
}
```

### Phase 1: Load JIT Cache

Load pre-compiled contracts via `compile_all_contracts_with_cache()`.

### Phase 2: Per-TX Ablation

For each tx:
1. Discover unique contracts touched (run once, record dispatched bytecode hashes)
2. Measure baselines (R rounds each): `t_jit` (full JIT) and `t_native` (no JIT)
3. For each contract `c`: measure `t_ablated_c` (JIT minus `{c}`) for R rounds

Ablation = filter the JIT HashMap to exclude one contract:
```rust
let ablated_fns: HashMap<B256, RawEvmCompilerFn> = all_functions
    .iter()
    .filter(|(&h, _)| h != target_hash)
    .map(|(&h, &f)| (h, f))
    .collect();
```

### Phase 3: Statistical Analysis

Welch's t-test (zero-dependency, ~60 lines):
- p < alpha && mean_diff > 0 → JIT-GOOD
- p < alpha && mean_diff < 0 → JIT-BAD
- p >= alpha → NEUTRAL

Two-level output:

**Per-TX detail:**
```
tx[0]: 3 contracts, jit_median=45.2us, native_median=52.1us
  Contract  Frames  Ablat(us)  Delta(us)  p-value  Cohen_d  Verdict
  0xabcd..    5      47.8       +2.6      0.003    1.2      JIT-GOOD
  0x1234..    1      44.9       -0.3      0.421    0.1      NEUTRAL
  0xdead..    2      43.1       -2.1      0.012    0.8      JIT-BAD
```

**Block summary:**
```
Contract    Appears  Frames  Avg_Delta(us)  Agg_p  Verdict
0xabcd..      12       45     +3.2          0.001  JIT-GOOD
0x1234..       8       12     -0.1          0.623  NEUTRAL
0xdead..       3        6     -1.8          0.008  JIT-BAD

JIT-GOOD: 15 contracts | JIT-BAD: 3 | NEUTRAL: 22
Optimal whitelist: 15 contracts -> predicted speedup 1.21x
```

## CLI Arguments

| Flag | Default | Description |
|------|---------|-------------|
| `--block` | required | Block number |
| `--cache-dir` | required | AOT cache directory |
| `--rounds` | 15 | Measurement rounds per configuration |
| `--warmup` | 3 | Untimed warmup runs |
| `--alpha` | 0.05 | Significance level for t-test |
| `--bench-dir` | `./data` | Directory with states/ and txs/ |
| `--verbose` | false | Print per-tx details |

## Reference

Based on `revm-hybrid-dispatch/crates/revm-ssa-integration-tests/src/bin/ablation_analysis.rs` which uses the same Welch's t-test approach for SSA path-level ablation.
