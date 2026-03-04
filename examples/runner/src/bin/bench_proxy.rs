//! Benchmark proxy-related transactions from a real block.
//!
//! Identifies transactions in the block that execute EIP-1167 minimal proxy
//! contracts (by tracing which bytecode hashes are executed per tx),
//! then benchmarks those transactions Native vs Full JIT.
//!
//! Usage:
//!   cargo run -p revmc-examples-runner --bin bench_proxy --release -- \
//!     --block 38004930 --cache-dir /tmp/jit_cache

#[allow(dead_code)]
#[path = "../bin_common.rs"]
mod bin_common;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use op_revm::{transaction::OpTransaction, DefaultOp};
use revm::{
    database::EmptyDB,
    handler::{EvmTr, FrameResult, Handler, ItemOrResult},
    primitives::B256,
};
use revmc::OptimizationLevel;
use revmc_builtins as _;
use revmc_context::RawEvmCompilerFn;

use bin_common::{
    build_op_cfg, build_op_tx, compile_all_contracts_with_cache, BenchEvm, BinLoader,
    JitHandler, NativeHandler, OpCtx,
};

// Target proxy bytecode hash prefixes (EIP-1167 proxies found in block 38004930)
const PROXY_PREFIXES: &[&str] = &[
    "acd671", // 45B → impl 772fb5 (24279B, 1.81x) — 251 frames
    "7dd6ff", // 45B → impl d22754 (21057B)         — 42 frames
    "183082", // 45B → impl 916bc5 (24279B)          — 28 frames
    "072cfd", // 45B → impl c4ba16 (18757B)          — 16 frames
    "6667ce", // 45B → impl e6ca90 (12613B)          — 8 frames
];

// ── Tracing handler: records which bytecode hashes are executed ──────────────

struct TracingHandler {
    executed: HashSet<B256>,
}

impl Handler for TracingHandler {
    type Evm = BenchEvm;
    type Error = bin_common::BenchError;
    type HaltReason = op_revm::OpHaltReason;

    fn run_exec_loop(
        &mut self,
        evm: &mut Self::Evm,
        first_frame_input: revm::interpreter::interpreter_action::FrameInit,
    ) -> Result<FrameResult, Self::Error> {
        let res = evm.frame_init(first_frame_input)?;
        if let ItemOrResult::Result(r) = res {
            return Ok(r);
        }
        loop {
            {
                let frame = evm.0.frame_stack.get();
                let hash = frame.interpreter.bytecode.get_or_calculate_hash();
                self.executed.insert(hash);
            }
            let call_or_result = evm.frame_run()?;
            let result = match call_or_result {
                ItemOrResult::Item(init) => match evm.frame_init(init)? {
                    ItemOrResult::Item(_) => continue,
                    ItemOrResult::Result(r) => r,
                },
                ItemOrResult::Result(r) => r,
            };
            if let Some(r) = evm.frame_return_result(result)? {
                return Ok(r);
            }
        }
    }
}

// ── Benchmark ────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "bench_proxy")]
struct Args {
    #[arg(long, default_value = "/home/ubuntu/sipeng/bench_data")]
    dir: String,
    #[arg(long, default_value_t = 38004930)]
    block: u64,
    #[arg(long, default_value = "/tmp/jit_cache")]
    cache_dir: String,
    /// Number of benchmark rounds
    #[arg(long, default_value_t = 7)]
    rounds: usize,
    /// Number of warmup rounds
    #[arg(long, default_value_t = 2)]
    warmup: usize,
    /// Benchmark all txs, not just proxy-related ones
    #[arg(long)]
    all_txs: bool,
}

/// Run selected txs, returning (total_duration, per_tx_durations).
fn run_selected_txs(
    loader: &BinLoader,
    tx_indices: &[usize],
    functions: Option<&Arc<HashMap<B256, RawEvmCompilerFn>>>,
) -> (Duration, Vec<Duration>) {
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

    let mut total = Duration::ZERO;
    let mut per_tx = Vec::with_capacity(tx_indices.len());
    for &i in tx_indices {
        let tx_bin = &loader.raw_txs()[i];
        if tx_bin.tx_type == 0x7e {
            per_tx.push(Duration::ZERO);
            continue;
        }
        evm.0.ctx.tx = build_op_tx(tx_bin);
        let t0 = Instant::now();
        if let Some(fns) = functions {
            let mut handler = JitHandler { functions: fns.clone() };
            let _ = handler.run(&mut evm);
        } else {
            let mut handler = NativeHandler;
            let _ = handler.run(&mut evm);
        }
        let elapsed = t0.elapsed();
        total += elapsed;
        per_tx.push(elapsed);
    }
    (total, per_tx)
}

fn main() {
    let args = Args::parse();
    let loader = BinLoader::new(Path::new(&args.dir), args.block).expect("load block");

    println!("=== bench_proxy: Block {} ===\n", args.block);

    // Phase 1: Load JIT cache
    eprintln!("Loading JIT cache from {} ...", args.cache_dir);
    let all_codes = {
        let mut m = HashMap::new();
        for (h, bc) in loader.code_values() {
            m.insert(*h, bc.clone());
        }
        m
    };
    let compiled = compile_all_contracts_with_cache(
        &all_codes,
        OptimizationLevel::Aggressive,
        Path::new(&args.cache_dir),
    );
    eprintln!("  {} JIT functions loaded\n", compiled.functions.len());

    // Phase 2: Identify proxy hashes
    let proxy_hashes: HashSet<B256> = compiled.functions.keys()
        .chain(loader.code_values().keys())
        .filter(|h| {
            let hex = hex::encode(h.as_slice());
            PROXY_PREFIXES.iter().any(|p| hex.starts_with(p))
        })
        .copied()
        .collect();

    println!("Target proxy contracts:");
    for h in &proxy_hashes {
        let size = loader.code_values().get(h)
            .map(|bc| bc.original_byte_slice().len())
            .unwrap_or(0);
        println!("  {} ({}B)", &hex::encode(h.as_slice())[..12], size);
    }
    println!();

    // Phase 3: Trace all txs to find proxy-touching ones
    eprintln!("Tracing {} txs to find proxy-related ones ...", loader.tx_count());
    let chain_id = loader.raw_txs().first().and_then(|tx| tx.chain_id);
    let cfg = build_op_cfg(chain_id);
    let dummy_tx = OpTransaction::builder().build_fill();
    let mut trace_evm: BenchEvm = {
        let ctx = OpCtx::<EmptyDB>::op()
            .with_cfg(cfg)
            .with_db(loader.build_cache_db())
            .with_tx(dummy_tx);
        op_revm::OpEvm::new(ctx, ())
    };

    let mut proxy_tx_indices: Vec<usize> = Vec::new();
    for (i, tx_bin) in loader.raw_txs().iter().enumerate() {
        if tx_bin.tx_type == 0x7e {
            continue;
        }
        trace_evm.0.ctx.tx = build_op_tx(tx_bin);
        let mut handler = TracingHandler { executed: HashSet::new() };
        let _ = handler.run(&mut trace_evm);
        if handler.executed.iter().any(|h| proxy_hashes.contains(h)) {
            proxy_tx_indices.push(i);
        }
    }

    println!("Found {}/{} txs that execute proxy contracts",
        proxy_tx_indices.len(), loader.tx_count());

    // Optionally use all non-deposit txs
    let selected_indices: Vec<usize> = if args.all_txs {
        println!("(--all-txs: benchmarking all non-deposit txs)\n");
        loader.raw_txs().iter().enumerate()
            .filter(|(_, tx)| tx.tx_type != 0x7e)
            .map(|(i, _)| i)
            .collect()
    } else {
        println!();
        if proxy_tx_indices.is_empty() {
            println!("No proxy transactions found.");
            return;
        }
        proxy_tx_indices
    };

    // Phase 4: Benchmark — aggregate + per-tx
    println!("Benchmarking {} txs ({} warmup + {} rounds) ...\n",
        selected_indices.len(), args.warmup, args.rounds);

    let n = selected_indices.len();
    let mut agg_native: Vec<Duration> = Vec::new();
    let mut agg_jit: Vec<Duration> = Vec::new();
    // per_tx_native[round][tx_idx] and per_tx_jit[round][tx_idx]
    let mut per_tx_native: Vec<Vec<Duration>> = Vec::new();
    let mut per_tx_jit: Vec<Vec<Duration>> = Vec::new();

    for round in 0..(args.warmup + args.rounds) {
        let (native_total, native_per) = run_selected_txs(&loader, &selected_indices, None);
        let (jit_total, jit_per) = run_selected_txs(&loader, &selected_indices, Some(&compiled.functions));
        if round >= args.warmup {
            agg_native.push(native_total);
            agg_jit.push(jit_total);
            per_tx_native.push(native_per);
            per_tx_jit.push(jit_per);
        }
    }

    agg_native.sort();
    agg_jit.sort();
    let native_median = agg_native[agg_native.len() / 2];
    let jit_median = agg_jit[agg_jit.len() / 2];
    let speedup = native_median.as_secs_f64() / jit_median.as_secs_f64();

    println!("========== Aggregate (median of {} rounds) ==========", args.rounds);
    println!("  Native:  {:.3}ms", native_median.as_secs_f64() * 1000.0);
    println!("  Full JIT:{:.3}ms", jit_median.as_secs_f64() * 1000.0);
    println!("  Speedup: {:.2}x", speedup);
    println!();

    // Per-tx: take median across rounds for each tx
    let rounds = per_tx_native.len();
    let mut per_tx_results: Vec<(usize, f64, f64, f64)> = Vec::new(); // (tx_idx, native_us, jit_us, speedup)
    for t in 0..n {
        let mut native_samples: Vec<u64> = (0..rounds)
            .map(|r| per_tx_native[r][t].as_nanos() as u64)
            .collect();
        let mut jit_samples: Vec<u64> = (0..rounds)
            .map(|r| per_tx_jit[r][t].as_nanos() as u64)
            .collect();
        native_samples.sort();
        jit_samples.sort();
        let n_us = native_samples[rounds / 2] as f64 / 1000.0;
        let j_us = jit_samples[rounds / 2] as f64 / 1000.0;
        if n_us < 0.5 { continue; } // skip near-zero (deposit/empty txs)
        let sp = if j_us > 0.0 { n_us / j_us } else { 0.0 };
        per_tx_results.push((selected_indices[t], n_us, j_us, sp));
    }

    // Sort by speedup ascending (slowest JIT first)
    per_tx_results.sort_by(|a, b| a.3.partial_cmp(&b.3).unwrap());

    let jit_slower = per_tx_results.iter().filter(|r| r.3 < 1.0).count();
    let jit_faster = per_tx_results.iter().filter(|r| r.3 > 1.0).count();
    println!("========== Per-Tx Analysis ({} txs with >0.5µs) ==========", per_tx_results.len());
    println!("  JIT faster: {}  JIT slower: {}  neutral: {}",
        jit_faster, jit_slower,
        per_tx_results.len() - jit_faster - jit_slower);
    println!();

    // Show worst 10 (JIT slowest)
    println!("--- Worst 10 txs for JIT (slowest first) ---");
    println!("  {:>6}  {:>9}  {:>9}  {:>7}  to", "tx_idx", "native_µs", "jit_µs", "speedup");
    for &(idx, n_us, j_us, sp) in per_tx_results.iter().take(10) {
        let to = loader.raw_txs()[idx].to
            .map(|a| format!("0x{}", hex::encode(&a[16..])))
            .unwrap_or_else(|| "CREATE".to_string());
        println!("  {:>6}  {:>9.1}  {:>9.1}  {:>6.2}x  {}", idx, n_us, j_us, sp, to);
    }
    println!();

    // Show best 10
    println!("--- Best 10 txs for JIT (fastest first) ---");
    println!("  {:>6}  {:>9}  {:>9}  {:>7}  to", "tx_idx", "native_µs", "jit_µs", "speedup");
    for &(idx, n_us, j_us, sp) in per_tx_results.iter().rev().take(10) {
        let to = loader.raw_txs()[idx].to
            .map(|a| format!("0x{}", hex::encode(&a[16..])))
            .unwrap_or_else(|| "CREATE".to_string());
        println!("  {:>6}  {:>9.1}  {:>9.1}  {:>6.2}x  {}", idx, n_us, j_us, sp, to);
    }
    println!();

    // Also show per-proxy-hash breakdown via frame_bench-style counting
    // Count how many times each proxy hash appears in traced txs
    let mut hash_call_counts: HashMap<B256, usize> = HashMap::new();
    {
        let chain_id = loader.raw_txs().first().and_then(|tx| tx.chain_id);
        let cfg = build_op_cfg(chain_id);
        let dummy_tx = OpTransaction::builder().build_fill();
        let mut count_evm: BenchEvm = {
            let ctx = OpCtx::<EmptyDB>::op()
                .with_cfg(cfg)
                .with_db(loader.build_cache_db())
                .with_tx(dummy_tx);
            op_revm::OpEvm::new(ctx, ())
        };
        for &i in &selected_indices {
            let tx_bin = &loader.raw_txs()[i];
            if tx_bin.tx_type == 0x7e { continue; }
            count_evm.0.ctx.tx = build_op_tx(tx_bin);
            let mut handler = TracingHandler { executed: HashSet::new() };
            let _ = handler.run(&mut count_evm);
            for h in handler.executed {
                if proxy_hashes.contains(&h) {
                    *hash_call_counts.entry(h).or_default() += 1;
                }
            }
        }
    }
    println!("Proxy contract appearances in selected txs:");
    let mut counts: Vec<_> = hash_call_counts.iter().collect();
    counts.sort_by_key(|(_, c)| std::cmp::Reverse(**c));
    for (h, c) in counts {
        println!("  {} txs_containing={}", &hex::encode(h.as_slice())[..12], c);
    }
}
