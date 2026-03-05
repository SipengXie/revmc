# Full-Block Ablation Implementation Plan

> **For Claude:** Execute this plan using the skill chosen during Execution Handoff (see end of plan).
> Planning dir: .planning/

**Goal:** Create `fullblock_ablation` binary that generates a JIT whitelist under realistic full-block L1i cache pressure.

**Architecture:** Single binary combining classify_slow_txs's full-block replay loop with ablation_bench's per-contract ablation and Welch's t-test. For each ablation config, replays the entire block and times every tx.

**Tech Stack:** Rust, clap, bin_common.rs infra, serde_json for whitelist output.

---

### Task 1: Scaffold binary with CLI and full-block replay

**Files:**
- Create: `examples/runner/src/bin/fullblock_ablation.rs`
- Modify: `examples/runner/Cargo.toml` (add `[[bin]]` entry)

**Step 1: Add bin entry to Cargo.toml**

Append before the `[[bench]]` section:

```toml
[[bin]]
name = "fullblock_ablation"
path = "src/bin/fullblock_ablation.rs"
```

**Step 2: Create binary with CLI, imports, and `run_full_block()`**

Create `examples/runner/src/bin/fullblock_ablation.rs` with:

```rust
//! Full-block JIT ablation: generate whitelist under realistic L1i pressure.
//!
//! Unlike ablation_bench (isolated per-tx snapshots), this tool replays the
//! entire block for each ablation config, capturing L1i cache effects.

#[allow(dead_code)]
#[path = "../bin_common.rs"]
mod bin_common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use op_revm::transaction::OpTransaction;
use op_revm::{DefaultOp, OpHaltReason};
use revm::{
    database::{CacheDB, EmptyDB},
    handler::{EvmTr, FrameResult, Handler, ItemOrResult},
    primitives::B256,
};
use revmc::OptimizationLevel;
use revmc_builtins as _;
use revmc_context::RawEvmCompilerFn;

use bin_common::{
    build_op_cfg, build_op_tx, compile_all_contracts_with_cache, run_jit_or_native,
    should_lookup_jit, BenchError, BenchEvm, BinLoader, JitHandler, NativeHandler, OpCtx,
};

#[derive(Parser)]
#[command(name = "fullblock_ablation", about = "Full-block JIT ablation for whitelist generation")]
struct Args {
    #[arg(long, default_value = "/home/ubuntu/sipeng/bench_data")]
    dir: String,
    #[arg(long)]
    block: u64,
    #[arg(long)]
    cache_dir: String,
    #[arg(long, default_value_t = 15)]
    rounds: usize,
    #[arg(long, default_value_t = 3)]
    warmup: usize,
    #[arg(long, default_value_t = 0.05)]
    alpha: f64,
    /// Path to write whitelist JSON
    #[arg(long)]
    output: Option<String>,
}

/// Replay full block, return per-tx durations (deposit txs get Duration::ZERO).
fn run_full_block(
    loader: &BinLoader,
    functions: Option<&Arc<HashMap<B256, RawEvmCompilerFn>>>,
) -> Vec<Duration> {
    let chain_id = loader.raw_txs().first().and_then(|tx| tx.chain_id);
    let cfg = build_op_cfg(chain_id);
    let dummy_tx = OpTransaction::builder().build_fill();
    let mut evm: BenchEvm = {
        let ctx = OpCtx::<EmptyDB>::op()
            .with_cfg(cfg)
            .with_db(loader.build_cache_db())
            .with_tx(dummy_tx);
        op_revm::OpEvm::new(ctx, ())
    };

    let mut per_tx = Vec::with_capacity(loader.tx_count());
    for tx_bin in loader.raw_txs() {
        if tx_bin.tx_type == 0x7e {
            per_tx.push(Duration::ZERO);
            continue;
        }
        evm.0.ctx.tx = build_op_tx(tx_bin);
        let t0 = Instant::now();
        if let Some(fns) = functions {
            let mut h = JitHandler { functions: fns.clone() };
            let _ = h.run(&mut evm);
        } else {
            let mut h = NativeHandler;
            let _ = h.run(&mut evm);
        }
        per_tx.push(t0.elapsed());
    }
    per_tx
}

fn main() {
    let args = Args::parse();
    let loader = BinLoader::new(Path::new(&args.dir), args.block).expect("load block");

    eprintln!("=== Full-Block Ablation: block {} ===", args.block);
    eprintln!("  rounds={}, warmup={}, alpha={}", args.rounds, args.warmup, args.alpha);

    // Load JIT
    let all_codes: HashMap<B256, _> = loader.code_values().iter()
        .map(|(h, b)| (*h, b.clone())).collect();
    let compiled = compile_all_contracts_with_cache(
        &all_codes, OptimizationLevel::Aggressive, Path::new(&args.cache_dir),
    );
    let all_functions = compiled.functions;
    eprintln!("  {} JIT functions loaded", all_functions.len());

    // TODO: Phase 0-3 in subsequent tasks
    eprintln!("  (scaffold only — phases not yet implemented)");
}
```

**Step 3: Build and verify**

Run: `cargo build -p revmc-examples-runner --bin fullblock_ablation --release 2>&1 | tail -3`
Expected: `Finished` with no errors.

**Step 4: Commit**

```bash
git add examples/runner/Cargo.toml examples/runner/src/bin/fullblock_ablation.rs
git commit -m "feat: scaffold fullblock_ablation binary with CLI and run_full_block"
```

> **Note:** Log discoveries to `.planning/findings.md`.

---

### Task 2: Implement Phase 0 (baseline) and Phase 1 (discovery)

**Files:**
- Modify: `examples/runner/src/bin/fullblock_ablation.rs`

**Step 1: Add DiscoveryHandler**

Copy the `DiscoveryHandler` from `ablation_bench.rs:101-180` verbatim. It works for full-block because it uses `run_jit_or_native` internally — just needs to be called per-tx in a full block loop instead of once per snapshot.

Add a full-block discovery function:

```rust
/// Run full block with discovery, return per-tx contract sets.
fn discover_full_block(
    loader: &BinLoader,
    all_functions: &Arc<HashMap<B256, RawEvmCompilerFn>>,
) -> Vec<Vec<(B256, usize)>> {
    let chain_id = loader.raw_txs().first().and_then(|tx| tx.chain_id);
    let cfg = build_op_cfg(chain_id);
    let dummy_tx = OpTransaction::builder().build_fill();
    let mut evm: BenchEvm = {
        let ctx = OpCtx::<EmptyDB>::op()
            .with_cfg(cfg)
            .with_db(loader.build_cache_db())
            .with_tx(dummy_tx);
        op_revm::OpEvm::new(ctx, ())
    };

    let mut per_tx_contracts = Vec::with_capacity(loader.tx_count());
    for tx_bin in loader.raw_txs() {
        if tx_bin.tx_type == 0x7e {
            per_tx_contracts.push(Vec::new());
            continue;
        }
        evm.0.ctx.tx = build_op_tx(tx_bin);
        let mut handler = DiscoveryHandler::new(all_functions.clone());
        let _ = handler.run(&mut evm);
        let counts: Vec<(B256, usize)> = handler.into_counts().into_iter().collect();
        per_tx_contracts.push(counts);
    }
    per_tx_contracts
}
```

**Step 2: Implement Phase 0 and Phase 1 in main**

Replace the TODO in `main()` with:

```rust
// Phase 0: Baseline
eprintln!("\n=== Phase 0: Baseline ===");
for _ in 0..args.warmup {
    run_full_block(&loader, None);
    run_full_block(&loader, Some(&all_functions));
}
let mut native_samples: Vec<Vec<Duration>> = Vec::with_capacity(args.rounds);
let mut jit_samples: Vec<Vec<Duration>> = Vec::with_capacity(args.rounds);
for r in 0..args.rounds {
    native_samples.push(run_full_block(&loader, None));
    jit_samples.push(run_full_block(&loader, Some(&all_functions)));
    if (r + 1) % 5 == 0 {
        eprintln!("  baseline round {}/{}", r + 1, args.rounds);
    }
}

// Compute per-tx medians
let n_tx = loader.tx_count();
let native_medians: Vec<f64> = (0..n_tx).map(|i| {
    median_us(&native_samples.iter().map(|s| s[i]).collect::<Vec<_>>())
}).collect();
let jit_medians: Vec<f64> = (0..n_tx).map(|i| {
    median_us(&jit_samples.iter().map(|s| s[i]).collect::<Vec<_>>())
}).collect();
let native_total: f64 = native_medians.iter().sum();
let jit_total: f64 = jit_medians.iter().sum();
eprintln!("  native={:.1}ms jit={:.1}ms ({:.2}x)",
    native_total / 1000.0, jit_total / 1000.0, native_total / jit_total);

// Phase 1: Discovery
eprintln!("\n=== Phase 1: Discovery ===");
let per_tx_contracts = discover_full_block(&loader, &all_functions);

// Build contract -> [tx_index] map
let mut contract_to_txs: HashMap<B256, Vec<usize>> = HashMap::new();
let mut contract_frames: HashMap<B256, usize> = HashMap::new();
for (i, contracts) in per_tx_contracts.iter().enumerate() {
    for &(hash, frame_count) in contracts {
        contract_to_txs.entry(hash).or_default().push(i);
        *contract_frames.entry(hash).or_insert(0) += frame_count;
    }
}
let unique_contracts: Vec<B256> = contract_to_txs.keys().copied().collect();
eprintln!("  {} unique JIT contracts across {} txs",
    unique_contracts.len(),
    per_tx_contracts.iter().filter(|c| !c.is_empty()).count());

// TODO: Phase 2-3 in next task
```

**Step 3: Add `median_us` and `to_us` helpers**

Copy from `ablation_bench.rs:391-403`:

```rust
fn median_us(v: &[Duration]) -> f64 {
    let mut us: Vec<f64> = v.iter().map(|d| d.as_secs_f64() * 1e6).collect();
    us.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if us.is_empty() { return 0.0; }
    us[us.len() / 2]
}

fn to_us(v: &[Duration]) -> Vec<f64> {
    v.iter().map(|d| d.as_secs_f64() * 1e6).collect()
}
```

**Step 4: Build and smoke test**

Run: `cargo build -p revmc-examples-runner --bin fullblock_ablation --release 2>&1 | tail -3`
Expected: Compiles.

Run: `cargo run -p revmc-examples-runner --bin fullblock_ablation --release -- --block 38004930 --cache-dir /tmp/jit_cache --rounds 3 --warmup 1 2>&1 | tail -10`
Expected: Shows baseline timing and discovery count, then hits TODO.

**Step 5: Commit**

```bash
git add examples/runner/src/bin/fullblock_ablation.rs
git commit -m "feat(fullblock_ablation): implement baseline and discovery phases"
```

---

### Task 3: Implement Phase 2 (ablation loop) and statistics

**Files:**
- Modify: `examples/runner/src/bin/fullblock_ablation.rs`

**Step 1: Copy statistics functions from ablation_bench**

Copy these functions verbatim from `ablation_bench.rs:258-413`:
- `WelchResult` struct
- `ln_gamma`, `regularized_incomplete_beta`, `t_cdf`
- `welch_t_test`
- `verdict`

**Step 2: Implement the ablation loop**

Replace the Phase 2-3 TODO with:

```rust
// Phase 2: Per-contract ablation
eprintln!("\n=== Phase 2: Per-contract ablation ({} contracts) ===", unique_contracts.len());

struct ContractResult {
    hash: B256,
    tx_count: usize,
    total_frames: usize,
    avg_delta_us: f64,
    p_value: f64,
    verdict: &'static str,
}

let mut results: Vec<ContractResult> = Vec::new();

for (ci, &contract_hash) in unique_contracts.iter().enumerate() {
    // Build ablated function map (exclude this contract)
    let ablated_fns: Arc<HashMap<B256, RawEvmCompilerFn>> = Arc::new(
        all_functions.iter()
            .filter(|(&h, _)| h != contract_hash)
            .map(|(&h, &f)| (h, f))
            .collect()
    );

    // Warmup
    for _ in 0..args.warmup {
        run_full_block(&loader, Some(&ablated_fns));
    }

    // Timed rounds
    let mut ablated_samples: Vec<Vec<Duration>> = Vec::with_capacity(args.rounds);
    for _ in 0..args.rounds {
        ablated_samples.push(run_full_block(&loader, Some(&ablated_fns)));
    }

    // Compare ablated vs all-JIT for each tx that touches this contract
    let affected_txs = &contract_to_txs[&contract_hash];
    let frames = contract_frames[&contract_hash];
    let mut deltas: Vec<f64> = Vec::new();

    for &tx_idx in affected_txs {
        let jit_us: Vec<f64> = jit_samples.iter().map(|s| s[tx_idx].as_secs_f64() * 1e6).collect();
        let abl_us: Vec<f64> = ablated_samples.iter().map(|s| s[tx_idx].as_secs_f64() * 1e6).collect();
        // delta = ablated - jit: positive means removing JIT made it slower (JIT helps)
        let jit_med = jit_us.iter().copied().sum::<f64>() / jit_us.len() as f64;
        let abl_med = abl_us.iter().copied().sum::<f64>() / abl_us.len() as f64;
        deltas.push(abl_med - jit_med);
    }

    // Aggregate: t-test on deltas vs zero
    let avg_delta = deltas.iter().sum::<f64>() / deltas.len().max(1) as f64;
    let (p_val, v) = if deltas.len() >= 2 {
        let zeros = vec![0.0f64; deltas.len()];
        if let Some(wr) = welch_t_test(&deltas, &zeros) {
            (wr.p_value, verdict(wr.p_value, wr.mean_diff, args.alpha))
        } else {
            (1.0, "NEUTRAL")
        }
    } else {
        let p = if deltas.first().map_or(false, |d| d.abs() > 1.0) { 0.01 } else { 1.0 };
        (p, verdict(p, deltas.first().copied().unwrap_or(0.0), args.alpha))
    };

    results.push(ContractResult {
        hash: contract_hash,
        tx_count: affected_txs.len(),
        total_frames: frames,
        avg_delta_us: avg_delta,
        p_value: p_val,
        verdict: v,
    });

    if (ci + 1) % 25 == 0 || ci + 1 == unique_contracts.len() {
        eprintln!("  ablated {}/{} contracts", ci + 1, unique_contracts.len());
    }
}

// TODO: Phase 3 reporting in next task
```

**Step 3: Build and verify**

Run: `cargo build -p revmc-examples-runner --bin fullblock_ablation --release 2>&1 | tail -3`
Expected: Compiles.

**Step 4: Commit**

```bash
git add examples/runner/src/bin/fullblock_ablation.rs
git commit -m "feat(fullblock_ablation): implement ablation loop with Welch's t-test"
```

---

### Task 4: Implement Phase 3 (reporting and JSON output)

**Files:**
- Modify: `examples/runner/src/bin/fullblock_ablation.rs`

**Step 1: Replace Phase 3 TODO with reporting**

```rust
// Phase 3: Report
results.sort_by(|a, b| b.avg_delta_us.partial_cmp(&a.avg_delta_us).unwrap());

println!("\n=== Full-Block Ablation: block {} ===\n", args.block);
println!("{:>16}  {:>5}  {:>6}  {:>13}  {:>8}  {}",
    "Contract", "Txs", "Frames", "Avg_Delta(us)", "p_value", "Verdict");

let mut n_good = 0usize;
let mut n_bad = 0usize;
let mut n_neutral = 0usize;
let mut whitelist: Vec<String> = Vec::new();
let mut blacklist: Vec<String> = Vec::new();

for r in &results {
    let short = &hex::encode(r.hash)[..12];
    println!("{short:>16}  {:>5}  {:>6}  {:>+13.1}  {:>8.4}  {}",
        r.tx_count, r.total_frames, r.avg_delta_us, r.p_value, r.verdict);
    match r.verdict {
        "JIT-GOOD" => {
            n_good += 1;
            whitelist.push(format!("0x{}", hex::encode(r.hash)));
        }
        "JIT-BAD" => {
            n_bad += 1;
            blacklist.push(format!("0x{}", hex::encode(r.hash)));
        }
        _ => n_neutral += 1,
    }
}

println!("\nJIT-GOOD: {n_good} | JIT-BAD: {n_bad} | NEUTRAL: {n_neutral}");
println!("Whitelist: {} contracts (JIT-GOOD only)", whitelist.len());

// JSON output
if let Some(output_path) = &args.output {
    let json = serde_json::json!({
        "block": args.block,
        "rounds": args.rounds,
        "alpha": args.alpha,
        "whitelist": whitelist,
        "blacklist": blacklist,
        "summary": {
            "good": n_good,
            "bad": n_bad,
            "neutral": n_neutral
        }
    });
    std::fs::write(output_path, serde_json::to_string_pretty(&json).unwrap())
        .expect("write whitelist JSON");
    eprintln!("Whitelist written to {output_path}");
}
```

**Step 2: Build and full run**

Run: `cargo build -p revmc-examples-runner --bin fullblock_ablation --release 2>&1 | tail -3`
Expected: Compiles.

Run:
```bash
cargo run -p revmc-examples-runner --bin fullblock_ablation --release -- \
  --block 38004930 --cache-dir /tmp/jit_cache \
  --rounds 15 --warmup 3 --output /tmp/fullblock_whitelist.json 2>&1 | tee /tmp/fullblock_ablation_r15.txt
```
Expected: Completes in ~3-5 minutes. Shows baseline, discovery, ablation progress, then table with verdicts and JSON file.

**Step 3: Validate output**

Run: `cat /tmp/fullblock_whitelist.json | python3 -m json.tool | head -20`
Expected: Valid JSON with whitelist/blacklist arrays.

Run: `tail -5 /tmp/fullblock_ablation_r15.txt`
Expected: `JIT-GOOD: N | JIT-BAD: N | NEUTRAL: N` with numbers summing to ~245.

**Step 4: Commit**

```bash
git add examples/runner/src/bin/fullblock_ablation.rs
git commit -m "feat(fullblock_ablation): add reporting and JSON whitelist output"
```

---

### Task 5: Validate and compare with isolated ablation

**Files:** None (analysis only)

**Step 1: Compare results**

Compare fullblock vs isolated ablation:
```bash
# Contracts that were GOOD in isolated but not in fullblock
diff <(grep 'JIT-GOOD' /tmp/ablation_revmc_r15.txt | awk '{print $1}' | sort) \
     <(grep 'JIT-GOOD' /tmp/fullblock_ablation_r15.txt | awk '{print $1}' | sort)
```

**Step 2: Update findings**

Log the comparison to `.planning/findings.md`: how many contracts flipped classification, and the overall whitelist size difference.

**Step 3: Update memory**

Update `memory/jit-performance.md` with fullblock ablation results.

---

### Parallelism Groups

- **Group A** (serial): Task 1 → Task 2 → Task 3 → Task 4
  - Sequential dependency: each task builds on the previous file state
- **Group B** (after Group A): Task 5
  - Requires completed binary + results

**Parallelism score:** 0/5 — strictly serial (single-file implementation).

**Recommendation:** Since all tasks are serial and touch the same file, use **Subagent-Driven** for sequential execution with review between tasks.
