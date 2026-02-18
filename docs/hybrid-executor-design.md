# Hybrid Executor Design: Adaptive JIT/Interpreter Dispatch

## 1. Executive Summary

### Data Source

All analysis in this document is based on real transaction data from **Base mainnet block #38014931** (JSONL file: `38014931.jsonl`). The block contains 406 transactions (6 skipped due to `EmptyAuthorizationList` errors), resulting in **400 transactions** used for benchmarking. Each transaction was executed with both the revm native interpreter (Native) and the revmc JIT compiler, under identical `CacheDB<EmptyDB>` (pure in-memory HashMap) state.

### Findings

Of the 400 transactions, JIT compilation benefits **70%** but harms the remaining **30%**. The current "always JIT" strategy yields only **1.024x** speedup. A **perfect oracle** — computed as `Σ min(native[i], jit[i])` over all 400 transactions — could achieve **1.190x**, representing **16.2% untapped potential**.

This document proposes a **hybrid executor** that dispatches each contract call frame to either JIT or interpreter based on runtime heuristics, targeting a realistic **1.10-1.15x** speedup.

### Performance Gap Visualization

```
Native baseline:  |████████████████████████████████████████| 48,599μs (1.000x)
Current JIT:      |███████████████████████████████████████ | 47,440μs (1.024x)
Hybrid (target):  |████████████████████████████████████    | ~43,000μs (1.13x)
Perfect oracle:   |█████████████████████████████████       | 40,832μs (1.190x)

* Perfect oracle = Σ min(native[i], jit[i]) for all 400 txs, theoretical upper bound
```

## 2. Data-Driven Analysis: Which Contracts Benefit from JIT?

### 2.1 Transaction Classification

| Category | Ratio Range | Count | % | Total Time Saved/Lost |
|----------|------------|-------|---|----------------------|
| JIT-Excellent | >1.8x | 20 | 5.0% | +2,841μs saved |
| JIT-Good | 1.3-1.8x | 156 | 39.0% | +4,932μs saved |
| JIT-Neutral | 0.8-1.3x | 145 | 36.3% | +406μs net |
| JIT-Harmful | <0.8x | 79 | 19.8% | -7,020μs lost |

### 2.2 JIT-Excellent Transactions (>1.8x Speedup) — Compute-Intensive

These are **the ideal JIT targets**:

| tx# | Native(μs) | JIT(μs) | Ratio | Gas | Pattern |
|-----|-----------|---------|-------|-----|---------|
| 142 | 42.8 | 4.7 | **9.06x** | 39,094 | Simple token op (outlier) |
| 323 | 58.8 | 24.6 | **2.39x** | 83,331 | Single-contract swap |
| 19 | 46.8 | 19.7 | **2.38x** | 66,069 | Token transfer |
| 391 | 78.5 | 35.2 | **2.23x** | 125,199 | DEX trade |
| 120 | 35.2 | 16.0 | **2.20x** | 59,397 | Token approve+transfer |
| 112 | 102.6 | 47.5 | **2.16x** | 131,106 | AMM swap |
| 303 | 106.3 | 49.8 | **2.13x** | 126,209 | AMM swap |
| 115 | 97.6 | 46.3 | **2.11x** | 131,106 | AMM swap |
| 110 | 102.3 | 49.9 | **2.05x** | 130,994 | AMM swap |
| 305 | 130.0 | 64.0 | **2.03x** | 201,551 | DEX aggregator |

**Common characteristics:**
- Gas: 39k-200k (median ~125k)
- Native time: 35-130μs
- **1-2 contract calls** (low frame count)
- **Arithmetic-heavy**: swaps, AMM curve calculations, balance updates
- Low SLOAD count relative to computation

### 2.3 JIT-Harmful Transactions (<0.5x) — Dispatch-Dominated

These are **contracts that should stay on the interpreter**:

| tx# | Native(μs) | JIT(μs) | Ratio | Gas | Pattern |
|-----|-----------|---------|-------|-----|---------|
| 237 | 17.4 | 94.3 | **0.18x** | 63,702 | Unknown multi-call |
| 224 | 11.4 | 53.1 | **0.21x** | 46,053 | Unknown multi-call |
| 24 | 24.4 | 92.3 | **0.26x** | 65,860 | Multi-hop routing |
| 3 | 12.3 | 45.7 | **0.27x** | 34,192 | MEV bot probe |
| 103 | 10.3 | 35.0 | **0.30x** | 34,000 | MEV bot probe |
| 210 | 18.7 | 62.5 | **0.30x** | 50,794 | Storage setter |
| 177 | 10.0 | 32.3 | **0.31x** | 43,923 | Multi-sig check |
| 41 | 18.7 | 58.4 | **0.32x** | 159,506 | Contract init |
| 385 | 54.2 | 167.4 | **0.32x** | 93,124 | Multi-contract |
| 98 | 49.9 | 145.5 | **0.34x** | 225,921 | Governance vote |
| 205 | 115.5 | 262.7 | **0.44x** | 168,419 | Diamond Proxy (89 SLOAD) |

**Common characteristics:**
- Many are **multi-contract calls** (>5 contract hops per tx)
- High **SLOAD/SSTORE count** relative to computation
- Some are **tiny transactions** (<15μs native) where JIT dispatch overhead (1-3μs/frame) dominates
- Proxy patterns (Diamond, multi-sig) with delegation chains

### 2.4 JIT Success Rate by Native Execution Time

```
 0-10μs:   ████████████████████  85.1% (47 txs) — simple ops, JIT inlines well
10-20μs:   █████████████         68.5% (54 txs) — mixed
20-30μs:   ██████████████        75.0% (40 txs) — mixed
30-50μs:   ███████████████       79.6% (49 txs) — sweet spot begins
50-100μs:  ██████████████        72.2% (97 txs) — good for compute-heavy
100-200μs: ██████████            54.5% (44 txs) — worst: multi-contract zone
200-300μs: ████████████          65.8% (38 txs) — large txs, mixed
300-500μs: ███████████           61.9% (21 txs) — complex txs
500+μs:    ███████               40.0% (10 txs) — very complex, many sub-calls
```

**Key insight**: There is no simple linear relationship. The 100-200μs and 500+μs ranges are worst for JIT because they correspond to **multi-contract interactions** where each sub-call pays JIT dispatch overhead.

## 3. Contract Type Classification for Executor Selection

### 3.1 JIT-Favorable Contract Types

| Contract Type | Examples | Why JIT Helps | Expected Speedup |
|--------------|---------|---------------|-----------------|
| **AMM/DEX Pools** | Uniswap V2/V3 Pool, Curve Pool | Tight arithmetic loops (sqrt, mulDiv), few external calls | 1.5-2.5x |
| **ERC-20 Transfers** | USDC, WETH, standard tokens | Simple: SLOAD balance, arithmetic, SSTORE | 1.2-1.7x |
| **Math Libraries** | PRBMath, FixedPointMath | Pure arithmetic, no external calls | 2.0-3.0x |
| **Simple Governance** | Single-contract voting | Minimal delegation, mostly storage + arithmetic | 1.3-1.6x |
| **NFT Mints** | ERC-721 mint() | Sequential ops, few branches | 1.2-1.5x |

**Bytecode signature**: High ratio of `ADD/MUL/SUB/DIV/SHL/SHR/AND/OR` to `CALL/STATICCALL/DELEGATECALL`.

### 3.2 Interpreter-Favorable Contract Types

| Contract Type | Examples | Why JIT Hurts | JIT Penalty |
|--------------|---------|---------------|------------|
| **Diamond Proxy (EIP-2535)** | RollDex, Aavegotchi | Deep DELEGATECALL chains, each hop = JIT dispatch | 0.3-0.5x |
| **Multi-sig Wallets** | Gnosis Safe `execTransaction()` | Signature verification + multiple guard calls | 0.4-0.5x |
| **MEV Bots** | Private bots, sandwich bots | Tiny probe calls, many reverts, multi-hop routing | 0.2-0.4x |
| **DEX Aggregators** | 1inch, Paraswap multi-hop | 5-15 sub-calls across different pools | 0.5-0.7x |
| **Account Abstraction** | ERC-4337 UserOp execution | Entrypoint → Wallet → Paymaster → target chain | 0.4-0.6x |
| **Proxy Patterns** | Minimal proxy (EIP-1167), UUPS | Extra DELEGATECALL per invocation | 0.6-0.8x |
| **Factory/Init** | Contract deployment, initialize() | One-time storage-heavy setup, no reuse benefit | 0.3-0.5x |

**Bytecode signature**: High ratio of `CALL/STATICCALL/DELEGATECALL` + high SLOAD/SSTORE count relative to arithmetic ops.

### 3.3 Concrete Examples from Block 38014931

**Best JIT candidates** (verified from benchmark):
```
tx#112: AMM swap      — 102.6μs → 47.5μs (2.16x) — Gas 131,106
tx#305: DEX trade     — 130.0μs → 64.0μs (2.03x) — Gas 201,551
tx#336: Token op      —  95.5μs → 54.9μs (1.74x) — Gas 126,221
tx#362: Heavy compute — 2209μs  → 1333μs (1.66x) — Gas 5,884,732
```

**Should use interpreter** (verified from benchmark):
```
tx#205: Diamond Proxy (RollDex)  — 115.5μs → 262.7μs (0.44x) — 89 SLOAD, 14 accounts
tx#6:   Gnosis Safe multisig     — 120.8μs → 244.3μs (0.49x) — 25 SLOAD, 15 accounts
tx#0:   MEV bot BrrrrrrrrrrrrrZ  — 150.6μs → 276.0μs (0.55x) — 31 SLOAD, 10 accounts
tx#237: Multi-call unknown       —  17.4μs →  94.3μs (0.18x) — high hop count
```

## 4. Hybrid Executor Architecture

### 4.1 Core Idea: Explore-then-Exploit

The fundamental challenge: we can only run **one** executor per frame, so we cannot directly compare JIT vs interpreter times for the same invocation. The solution is an **explore-then-exploit** strategy inspired by multi-armed bandit algorithms.

**Safety guarantee**: JIT and interpreter produce **identical results** (same gas, return values, state changes) for any contract. The only difference is speed. Therefore alternating between them during exploration is always safe — it never affects correctness.

**Per-bytecode lifecycle:**

```
   First encounter          N encounters             After N samples
   ┌──────────┐          ┌────────────────┐        ┌──────────────┐
   │ Phase 1  │──pass──▶ │    Phase 3     │──────▶ │    Locked    │
   │ Static   │          │  Exploration   │        │   Decision   │
   │ Filter   │          │  (alternate    │        │  (exploit)   │
   │          │          │   JIT/interp)  │        │              │
   └────┬─────┘          └────────────────┘        └──────────────┘
        │ reject
        │ (proxy / obvious bad pattern)
        ▼
   Interpreter only (permanent)
```

- **Phase 1** (compile-time): Static bytecode analysis. Hard-reject contracts that are obviously bad for JIT. One-time cost per bytecode, cached alongside compiled functions.
- **Phase 2** (first dispatch): Static score from Phase 1 determines the **initial exploration order** — whether JIT or interpreter goes first during the exploration window.
- **Phase 3** (runtime, cross-block): Alternate between JIT and interpreter for `N` rounds, measure actual wall-clock times for both, then lock in the faster one permanently.

### 4.2 Data Structures

```rust
/// Stored alongside each compiled JIT function (computed once at compile time).
struct CompiledContract {
    jit_fn: RawEvmCompilerFn,
    profile: OpcodeProfile,
    static_score: f32,        // Phase 2: pre-computed heuristic score
    skip_jit: bool,           // Phase 1: hard filter (proxy, etc.)
}

/// Static bytecode features (Phase 1).
struct OpcodeProfile {
    arithmetic_ratio: f32,    // (ADD+MUL+SUB+DIV+SHL+SHR) / total_opcodes
    call_count: u16,          // Number of CALL/STATICCALL/DELEGATECALL in bytecode
    sload_count: u16,         // Number of SLOAD instructions
    is_proxy: bool,           // Starts with DELEGATECALL pattern (EIP-1167, etc.)
}

/// Runtime performance record per bytecode hash (Phase 3).
struct PerfRecord {
    jit_times_ns: Vec<u64>,       // measured JIT execution times
    interp_times_ns: Vec<u64>,    // measured interpreter execution times
    decision: Option<bool>,       // None = still exploring, Some(true) = use JIT
}

/// The hybrid handler replacing the current JitHandler.
struct HybridHandler {
    compiled: Arc<HashMap<B256, CompiledContract>>,  // Phase 1 + 2
    perf: HashMap<B256, PerfRecord>,                  // Phase 3
}
```

### 4.3 Phase 1 — Static Bytecode Analysis (Compile-Time Hard Filter)

Performed once per unique bytecode during JIT compilation. Identifies contracts that should **never** use JIT.

```rust
fn analyze_bytecode(bytecode: &[u8]) -> OpcodeProfile {
    let mut arith = 0u32;
    let mut calls = 0u16;
    let mut sloads = 0u16;
    let mut total = 0u32;
    let mut is_proxy = false;

    // EIP-1167 minimal proxy: 0x363d3d373d3d3d363d73...
    if bytecode.len() > 10 {
        is_proxy = bytecode.starts_with(&[0x36, 0x3d, 0x3d, 0x37]);
    }

    let mut i = 0;
    while i < bytecode.len() {
        let op = bytecode[i];
        total += 1;
        match op {
            0x01..=0x0b | 0x10..=0x1d => arith += 1, // ADD..SIGNEXTEND, LT..SAR
            0xf1 | 0xf2 | 0xf4 | 0xfa => calls += 1, // CALL, CALLCODE, DELEGATECALL, STATICCALL
            0x54 => sloads += 1,                        // SLOAD
            0x60..=0x7f => i += (op - 0x5f) as usize,  // Skip PUSH data
            _ => {}
        }
        i += 1;
    }

    OpcodeProfile {
        arithmetic_ratio: if total > 0 { arith as f32 / total as f32 } else { 0.0 },
        call_count: calls,
        sload_count: sloads,
        is_proxy,
    }
}

/// Hard filter: returns true if JIT should be permanently skipped.
fn should_skip_jit(profile: &OpcodeProfile) -> bool {
    // EIP-1167 minimal proxy — pure DELEGATECALL forwarder
    if profile.is_proxy {
        return true;
    }
    // Bytecode that is almost entirely CALL/DELEGATECALL with no arithmetic
    if profile.call_count > 5 && profile.arithmetic_ratio < 0.05 {
        return true;
    }
    false
}
```

### 4.4 Phase 2 — Static Score (Exploration Order Bias)

Phase 2 does **not** make the final decision. It only determines which executor to try **first** during Phase 3 exploration. High-score contracts explore JIT first; low-score contracts explore interpreter first.

```rust
fn compute_static_score(profile: &OpcodeProfile) -> f32 {
    let mut score: f32 = 0.0;

    // Positive signals (favor JIT):
    score += profile.arithmetic_ratio * 2.0;

    // Negative signals (favor interpreter):
    score -= (profile.call_count as f32) * 0.3;
    score -= if profile.sload_count > 20 { 0.5 } else { 0.0 };

    score
}
```

This score is computed once at compile time and stored in `CompiledContract::static_score`.

### 4.5 Phase 3 — Explore-then-Exploit (Runtime Adaptive Learning)

The core adaptive mechanism. For each bytecode hash, alternate between JIT and interpreter for `EXPLORE_ROUNDS` times each, measure wall-clock times, then lock in the winner.

```rust
const EXPLORE_ROUNDS: usize = 3; // 3 JIT + 3 interp = 6 total before locking

fn choose_executor(
    compiled: &CompiledContract,
    record: &mut PerfRecord,
) -> ExecutorChoice {
    // Phase 1: hard filter — permanent interpreter
    if compiled.skip_jit {
        return ExecutorChoice::Interpreter;
    }

    // Phase 3: already locked — use cached decision
    if let Some(use_jit) = record.decision {
        return if use_jit { ExecutorChoice::Jit } else { ExecutorChoice::Interpreter };
    }

    let jit_n = record.jit_times_ns.len();
    let interp_n = record.interp_times_ns.len();
    let total = jit_n + interp_n;

    // Enough samples collected — lock decision
    if jit_n >= EXPLORE_ROUNDS && interp_n >= EXPLORE_ROUNDS {
        let avg_jit = record.jit_times_ns.iter().sum::<u64>() as f64 / jit_n as f64;
        let avg_interp = record.interp_times_ns.iter().sum::<u64>() as f64 / interp_n as f64;
        let use_jit = avg_jit < avg_interp;
        record.decision = Some(use_jit);
        return if use_jit { ExecutorChoice::Jit } else { ExecutorChoice::Interpreter };
    }

    // Still exploring — alternate, biased by Phase 2 static score.
    // High static_score → JIT on even rounds, interp on odd rounds.
    // Low static_score  → interp on even rounds, JIT on odd rounds.
    let jit_first = compiled.static_score > 0.0;
    let pick_jit = if jit_first { total % 2 == 0 } else { total % 2 == 1 };

    // Ensure we collect enough samples for both sides
    if pick_jit && jit_n >= EXPLORE_ROUNDS {
        ExecutorChoice::Interpreter
    } else if !pick_jit && interp_n >= EXPLORE_ROUNDS {
        ExecutorChoice::Jit
    } else if pick_jit {
        ExecutorChoice::Jit
    } else {
        ExecutorChoice::Interpreter
    }
}
```

**Measurement**: Wrap each frame execution in `Instant::now()` / `elapsed()`:

```rust
let start = Instant::now();
let result = match choice {
    ExecutorChoice::Jit => {
        let f = EvmCompilerFn::new(compiled.jit_fn);
        unsafe { f.call_with_interpreter(&mut frame.interpreter, ctx) }
    }
    ExecutorChoice::Interpreter => {
        drop((ctx, _instructions, _precompiles, frame_stack));
        evm.frame_run()?
    }
};
let elapsed_ns = start.elapsed().as_nanos() as u64;

// Record measurement
match choice {
    ExecutorChoice::Jit => record.jit_times_ns.push(elapsed_ns),
    ExecutorChoice::Interpreter => record.interp_times_ns.push(elapsed_ns),
}
```

### 4.6 Cross-Block Persistence

**Key constraint**: In the benchmark (single block), most contracts appear only 1-3 times per block — not enough for `EXPLORE_ROUNDS=3` to converge within one block. The exploration window spans **multiple blocks**.

| Deployment Scenario | Strategy |
|---|---|
| **Single-block benchmark** | Phase 1 hard filter + Phase 2 static score only. Phase 3 disabled (not enough samples). |
| **Multi-block benchmark** | Full 3-phase. `PerfRecord` persists across blocks. Typical contract converges in 2-4 blocks. |
| **Production node (Helios)** | Full 3-phase. `PerfRecord` stored in bounded LRU cache (e.g., 10k entries). Contracts recur across blocks naturally. Periodically re-evaluate locked decisions (every ~1000 encounters) to handle code upgrades or changed access patterns. |

```rust
/// For production: bounded LRU cache with periodic re-evaluation.
struct PerfHistory {
    records: LruCache<B256, PerfRecord>,  // bounded to MAX_ENTRIES
}

impl PerfHistory {
    fn get_or_create(&mut self, hash: B256) -> &mut PerfRecord {
        if !self.records.contains(&hash) {
            self.records.put(hash, PerfRecord::default());
        }
        self.records.get_mut(&hash).unwrap()
    }
}
```

### 4.7 Full Dispatch Flow

Putting it all together — the complete decision flow in `HybridHandler::run_exec_loop`:

```rust
loop {
    let call_or_result = {
        let (ctx, _instructions, _precompiles, frame_stack) = evm.all_mut();
        let frame = frame_stack.get();
        let bytecode_hash = frame.interpreter.bytecode.get_or_calculate_hash();

        // Look up compiled contract (includes Phase 1 profile + Phase 2 score)
        if let Some(compiled) = self.compiled.get(&bytecode_hash) {
            let record = self.perf.entry(bytecode_hash).or_default();
            let choice = choose_executor(compiled, record);

            let start = Instant::now();
            let action = match choice {
                ExecutorChoice::Jit => {
                    let f = EvmCompilerFn::new(compiled.jit_fn);
                    unsafe { f.call_with_interpreter(&mut frame.interpreter, ctx) }
                }
                ExecutorChoice::Interpreter => {
                    drop((ctx, _instructions, _precompiles, frame_stack));
                    return /* run interpreter frame and continue */;
                }
            };
            let elapsed_ns = start.elapsed().as_nanos() as u64;

            // Record measurement for Phase 3
            match choice {
                ExecutorChoice::Jit => record.jit_times_ns.push(elapsed_ns),
                ExecutorChoice::Interpreter => record.interp_times_ns.push(elapsed_ns),
            }

            frame.process_next_action::<_, BenchError>(ctx, action)
                .inspect(|i| { if i.is_result() { frame.set_finished(true); } })?
        } else {
            // No compiled function — always interpreter
            drop((ctx, _instructions, _precompiles, frame_stack));
            evm.frame_run()?
        }
    };
    // ... rest of frame loop unchanged ...
}
```

## 5. Implementation Plan

### Phase 1: Bytecode Profiling + Proxy Detection (1-2 days)

Add `OpcodeProfile` computation during JIT compilation. Skip JIT for detected proxy patterns.

**Expected gain**: Avoid the worst cases (0.18x-0.45x). Estimate **+2-3% overall speedup** by eliminating ~15 worst transactions.

**Changes:**
- `crates/revmc/src/compiler/translate.rs`: Add static analysis pass
- `examples/runner/src/bin/jsonl_bench.rs`: Add `OpcodeProfile` to `JitHandler`

### Phase 2: Frame-Level Dispatch Heuristic (2-3 days)

Implement `compute_static_score()`. Use the score as initial bias for new contracts before Phase 3 data is available.

**Expected gain**: Correctly route ~80% of the 30% harmful transactions to interpreter. Estimate **+5-8% overall speedup**.

**Changes:**
- `examples/runner/src/bin/jsonl_bench.rs`: Replace simple HashMap lookup with heuristic dispatch
- New: `crates/revmc-context/src/heuristic.rs`

### Phase 3: Explore-then-Exploit (3-5 days)

Implement alternating execution with timing. Add `PerfRecord` per bytecode hash, lock decision after `EXPLORE_ROUNDS` samples.

**Expected gain**: Approach oracle-level decisions for repeated contracts. Estimate **+3-5% additional speedup**.

**Changes:**
- New: `crates/revmc-context/src/perf_history.rs`
- `HybridHandler`: Add `PerfHistory` field, record timing after each frame

### Phase 4: Validation + Tuning (2-3 days)

Run against multiple blocks, tune `EXPLORE_ROUNDS` and score weights, validate no regressions.

### Expected Cumulative Results

| Phase | Speedup | Improvement vs Baseline |
|-------|---------|------------------------|
| Baseline (always JIT) | 1.024x | — |
| + Phase 1 (proxy skip) | ~1.05x | +2.5% |
| + Phase 2 (static heuristic) | ~1.10x | +7.5% |
| + Phase 3 (adaptive) | ~1.13x | +10% |
| Perfect oracle (upper bound) | 1.190x | +16.2% |

## 6. Risk Analysis

| Risk | Mitigation |
|------|-----------|
| Exploration overhead (first N runs are suboptimal) | N=3 per side = 6 total runs. Tiny cost amortized over contract lifetime. |
| `Instant::now()` timing noise on short frames | Use `EXPLORE_ROUNDS=3` and average to smooth out noise. |
| Heuristic mis-classifies during Phase 2 | Phase 2 only affects exploration **order**, not final decision. Phase 3 measurement corrects any bias. |
| OpcodeProfile analysis overhead | One-time per bytecode, cached alongside JIT compilation. |
| PerfHistory memory growth | Bounded LRU cache (e.g., 10k entries). |
| Same contract, different behavior across inputs | `EXPLORE_ROUNDS=3` captures some variance. For production, periodic re-evaluation (every ~1000 encounters) handles drift. |
| Single-block benchmark can't converge Phase 3 | Fallback: use Phase 1+2 only for single-block mode. |

## 7. Open Questions

1. **Optimal `EXPLORE_ROUNDS` value?**
   - Too low (1-2): noisy measurements, wrong decisions
   - Too high (10+): too much exploration cost before converging
   - Recommended start: 3 (6 total executions per contract before locking)

2. **Should locked decisions be re-evaluated?**
   - In production: yes, periodically (every ~1000 encounters) to handle upgraded contracts or changed access patterns
   - In benchmarks: no, lock permanently after convergence

3. **Frame-level vs transaction-level timing?**
   - Current design: frame level (each contract call independently timed)
   - Pro: granular, handles cases where inner calls benefit from JIT but outer calls don't
   - Con: `Instant::now()` overhead per frame (~20ns) adds up for many-frame transactions

---

**Related Documents:**
- [JIT Performance Analysis](./jit-performance-analysis.md) — Root cause analysis of 1.02x speedup
- Benchmark data: `jsonl_bench_codecopy_fix.log`
- JIT dispatch code: `examples/runner/src/bin/jsonl_bench.rs:674-730`

**Analysis data:**
- Charts: `~/.omc/scientist/figures/jit_summary_dashboard.png`
- Detailed report: `~/.omc/scientist/reports/jit_performance_analysis_report.md`
