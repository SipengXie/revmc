//! Multi-block JIT benchmark: revmc JIT vs native revm interpreter.
//!
//! Loads prestate from CacheSnapshot (.bin) and transactions from BlockBin (.bin),
//! then compiles all unique contracts via LLVM JIT and benchmarks execution.
//!
//! Usage:
//!   cargo run -p revmc-examples-runner --bin bin_bench --release -- --start 38004930 --count 10
//!   cargo run -p revmc-examples-runner --bin bin_bench --release -- --start 38004930 --end 38004940
//!   cargo run -p revmc-examples-runner --bin bin_bench --release -- --start 38004930 --count 5 --cache-dir /tmp/jit_cache

#[allow(dead_code)]
#[path = "../bin_common.rs"]
mod bin_common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use op_revm::transaction::OpTransaction;
use op_revm::DefaultOp;
use revm::{
    database::EmptyDB,
    handler::Handler,
    primitives::B256,
};
use revmc::OptimizationLevel;
use revmc_builtins as _;
use revmc_context::RawEvmCompilerFn;

use bin_common::{
    build_op_cfg, build_op_tx, collect_unique_bytecodes, compile_all_contracts,
    compile_all_contracts_with_cache, extract_gas, read_jit_dispatch_stats, reset_jit_dispatch_stats,
    set_jit_dispatch_stats_enabled, BenchEvm, BinLoader, JitDispatchStats, JitHandler,
    NativeHandler, OpCtx,
};

// ── Block Execution ─────────────────────────────────────────────────────────

struct BlockResult {
    native_results: Vec<(bool, u64)>,
    jit_results: Vec<(bool, u64)>,
    native_dur: Duration,
    jit_dur: Duration,
    jit_dispatch: JitDispatchStats,
}

/// Run a full block: for each tx, execute native then JIT on independent EVMs.
/// Both EVMs accumulate state across txs (nonce, balance, storage updates).
fn run_block(
    loader: &BinLoader,
    functions: &Arc<HashMap<B256, RawEvmCompilerFn>>,
    collect_dispatch_stats: bool,
) -> Result<BlockResult, String> {
    let chain_id = loader.raw_txs().first().and_then(|tx| tx.chain_id);
    let cfg = build_op_cfg(chain_id);

    let dummy_tx = OpTransaction::builder().build_fill();
    let mut native_evm: BenchEvm = {
        let ctx = OpCtx::<EmptyDB>::op()
            .with_cfg(cfg.clone())
            .with_db(loader.build_cache_db())
            .with_tx(dummy_tx.clone());
        op_revm::OpEvm::new(ctx, ())
    };
    let mut jit_evm: BenchEvm = {
        let ctx = OpCtx::<EmptyDB>::op()
            .with_cfg(cfg)
            .with_db(loader.build_cache_db())
            .with_tx(dummy_tx);
        op_revm::OpEvm::new(ctx, ())
    };

    let mut native_results = Vec::with_capacity(loader.tx_count());
    let mut jit_results = Vec::with_capacity(loader.tx_count());
    let mut native_dur = Duration::ZERO;
    let mut jit_dur = Duration::ZERO;
    if collect_dispatch_stats {
        reset_jit_dispatch_stats();
    }

    for (i, tx_bin) in loader.raw_txs().iter().enumerate() {
        if tx_bin.tx_type == 0x7e {
            native_results.push((true, 0));
            jit_results.push((true, 0));
            continue;
        }
        let op_tx = build_op_tx(tx_bin);

        native_evm.0.ctx.tx = op_tx.clone();
        let mut handler = NativeHandler;
        let t0 = Instant::now();
        match handler.run(&mut native_evm) {
            Ok(result) => {
                native_dur += t0.elapsed();
                native_results.push(extract_gas(&result));
            }
            Err(e) => {
                native_dur += t0.elapsed();
                eprintln!("  native tx[{i}] error: {e:?}");
                native_results.push((false, 0));
            }
        }

        jit_evm.0.ctx.tx = op_tx;
        let mut handler = JitHandler {
            functions: functions.clone(),
        };
        let t0 = Instant::now();
        match handler.run(&mut jit_evm) {
            Ok(result) => {
                jit_dur += t0.elapsed();
                jit_results.push(extract_gas(&result));
            }
            Err(e) => {
                jit_dur += t0.elapsed();
                eprintln!("  jit tx[{i}] error: {e:?}");
                jit_results.push((false, 0));
            }
        }
    }

    Ok(BlockResult {
        native_results,
        jit_results,
        native_dur,
        jit_dur,
        jit_dispatch: if collect_dispatch_stats {
            read_jit_dispatch_stats()
        } else {
            JitDispatchStats::default()
        },
    })
}

// ── Statistics ──────────────────────────────────────────────────────────────

struct BlockStats {
    block_number: u64,
    tx_count: usize,
    native_dur: Duration,
    jit_dur: Duration,
    total_native_gas: u64,
    total_jit_gas: u64,
    mismatches: usize,
    jit_dispatch: JitDispatchStats,
}

impl BlockStats {
    fn speedup(&self) -> f64 {
        self.native_dur.as_secs_f64() / self.jit_dur.as_secs_f64()
    }

    fn print_summary(&self, show_dispatch: bool) {
        println!(
            "Block {} | {:>3} txs | native {:.2}ms | jit {:.2}ms | {:.2}x | gas n={} j={} | {}",
            self.block_number,
            self.tx_count,
            self.native_dur.as_secs_f64() * 1000.0,
            self.jit_dur.as_secs_f64() * 1000.0,
            self.speedup(),
            self.total_native_gas,
            self.total_jit_gas,
            if self.mismatches > 0 {
                format!("{} MISMATCH", self.mismatches)
            } else {
                "OK".into()
            },
        );
        if show_dispatch {
            println!(
                "  JIT dispatch: frames={} lookup={} (hit={} miss={}) skipped: create={} no_bytecode_addr={} empty_bytecode={}",
                self.jit_dispatch.total_frames,
                self.jit_dispatch.lookup_attempts,
                self.jit_dispatch.lookup_hits,
                self.jit_dispatch.lookup_misses,
                self.jit_dispatch.skip_create,
                self.jit_dispatch.skip_no_bytecode_address,
                self.jit_dispatch.skip_empty_bytecode,
            );
        }
    }
}

// ── CLI + Main ──────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "bin_bench",
    about = "Multi-block JIT benchmark: Native vs revmc JIT"
)]
struct Args {
    /// Path to bench_data directory
    #[arg(long, default_value = "/home/ubuntu/sipeng/bench_data")]
    dir: String,

    /// First block number
    #[arg(long, default_value_t = 38004930)]
    start: u64,

    /// Last block number (inclusive, overrides --count)
    #[arg(long)]
    end: Option<u64>,

    /// Number of blocks to run (from --start)
    #[arg(long, default_value_t = 10)]
    count: u64,

    /// Step between sampled blocks (e.g. 1000 = every 1000th block)
    #[arg(long, default_value_t = 1000)]
    step: u64,

    /// Persistent AOT cache directory
    #[arg(long)]
    cache_dir: Option<String>,

    /// Print temporary JIT dispatch diagnostics (adds measurement overhead)
    #[arg(long)]
    dispatch_stats: bool,
}

fn main() {
    let args = Args::parse();
    set_jit_dispatch_stats_enabled(args.dispatch_stats);
    let bench_dir = Path::new(&args.dir);
    let block_range: Vec<u64> = if let Some(end) = args.end {
        (args.start..=end).collect()
    } else {
        (0..args.count)
            .map(|i| args.start + i * args.step)
            .collect()
    };

    println!(
        "=== Bin JIT Benchmark: {} blocks (start={}, step={}) ===\n",
        block_range.len(),
        args.start,
        args.step,
    );

    // Phase 1: Load all blocks
    eprintln!("=== Loading blocks ===");
    let load_start = Instant::now();
    let loaders: Vec<BinLoader> = block_range
        .iter()
        .filter_map(|&bn| match BinLoader::new(bench_dir, bn) {
            Ok(l) => {
                eprintln!(
                    "  block {bn}: {} txs, {} accounts, {} codes",
                    l.tx_count(),
                    l.account_count(),
                    l.code_count()
                );
                Some(l)
            }
            Err(e) => {
                eprintln!("  skip block {bn}: {e}");
                None
            }
        })
        .collect();
    eprintln!(
        "  Loaded {} blocks in {:.2}s\n",
        loaders.len(),
        load_start.elapsed().as_secs_f64()
    );

    if loaders.is_empty() {
        eprintln!("No blocks loaded.");
        return;
    }

    // Phase 2: Collect unique bytecodes and compile
    eprintln!("=== Compiling contracts ===");
    let all_codes = collect_unique_bytecodes(&loaders);
    eprintln!("  Unique bytecodes across all blocks: {}", all_codes.len());

    let compile_start = Instant::now();
    let compiled = if let Some(ref cache_dir) = args.cache_dir {
        compile_all_contracts_with_cache(
            &all_codes,
            OptimizationLevel::Aggressive,
            Path::new(cache_dir),
        )
    } else {
        compile_all_contracts(&all_codes)
    };
    let compile_dur = compile_start.elapsed();
    eprintln!("  Compilation time: {:.2}s\n", compile_dur.as_secs_f64());

    // Phase 3: Execute blocks
    eprintln!("=== Benchmark: Native vs JIT ===");
    let mut all_stats: Vec<BlockStats> = Vec::new();
    let mut skipped = 0u64;

    for loader in &loaders {
        let block_result = match run_block(loader, &compiled.functions, args.dispatch_stats) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("FAIL block {}: {e}", loader.block_number());
                skipped += 1;
                continue;
            }
        };

        let mut mismatches = 0;
        for i in 0..loader.tx_count() {
            let (_, n_gas) = block_result.native_results[i];
            let (_, j_gas) = block_result.jit_results[i];
            if n_gas != j_gas {
                eprintln!(
                    "  GAS MISMATCH block {} tx[{i}]: native={n_gas} jit={j_gas}",
                    loader.block_number()
                );
                mismatches += 1;
            }
        }

        let stats = BlockStats {
            block_number: loader.block_number(),
            tx_count: loader.tx_count(),
            native_dur: block_result.native_dur,
            jit_dur: block_result.jit_dur,
            total_native_gas: block_result.native_results.iter().map(|(_, g)| g).sum(),
            total_jit_gas: block_result.jit_results.iter().map(|(_, g)| g).sum(),
            mismatches,
            jit_dispatch: block_result.jit_dispatch,
        };
        stats.print_summary(args.dispatch_stats);
        all_stats.push(stats);
    }

    // Phase 4: Aggregate
    if all_stats.is_empty() {
        println!("\nNo blocks completed.");
        return;
    }

    let total_txs: usize = all_stats.iter().map(|s| s.tx_count).sum();
    let total_native: Duration = all_stats.iter().map(|s| s.native_dur).sum();
    let total_jit: Duration = all_stats.iter().map(|s| s.jit_dur).sum();
    let total_native_gas: u64 = all_stats.iter().map(|s| s.total_native_gas).sum();
    let total_jit_gas: u64 = all_stats.iter().map(|s| s.total_jit_gas).sum();
    let total_mismatches: usize = all_stats.iter().map(|s| s.mismatches).sum();
    let total_dispatch = if args.dispatch_stats {
        Some(all_stats.iter().fold(JitDispatchStats::default(), |mut acc, s| {
            acc.total_frames += s.jit_dispatch.total_frames;
            acc.lookup_attempts += s.jit_dispatch.lookup_attempts;
            acc.lookup_hits += s.jit_dispatch.lookup_hits;
            acc.lookup_misses += s.jit_dispatch.lookup_misses;
            acc.skip_create += s.jit_dispatch.skip_create;
            acc.skip_no_bytecode_address += s.jit_dispatch.skip_no_bytecode_address;
            acc.skip_empty_bytecode += s.jit_dispatch.skip_empty_bytecode;
            acc
        }))
    } else {
        None
    };

    println!("\n========== Aggregate ==========");
    println!(
        "Blocks: {} ok, {skipped} skipped | Txs: {total_txs}",
        all_stats.len(),
    );
    println!(
        "Compile:   {:.2}s (one-time, shared across all blocks)",
        compile_dur.as_secs_f64(),
    );
    println!(
        "Native:    {:.2}ms ({:.2} ms/block)",
        total_native.as_secs_f64() * 1000.0,
        total_native.as_secs_f64() * 1000.0 / all_stats.len() as f64,
    );
    println!(
        "JIT:       {:.2}ms ({:.2} ms/block)",
        total_jit.as_secs_f64() * 1000.0,
        total_jit.as_secs_f64() * 1000.0 / all_stats.len() as f64,
    );
    println!(
        "Speedup:   {:.2}x (native/jit)",
        total_native.as_secs_f64() / total_jit.as_secs_f64(),
    );
    if let Some(total_dispatch) = total_dispatch {
        println!(
            "Dispatch:  frames={} lookup={} (hit={} miss={}) skipped: create={} no_bytecode_addr={} empty_bytecode={}",
            total_dispatch.total_frames,
            total_dispatch.lookup_attempts,
            total_dispatch.lookup_hits,
            total_dispatch.lookup_misses,
            total_dispatch.skip_create,
            total_dispatch.skip_no_bytecode_address,
            total_dispatch.skip_empty_bytecode,
        );
    }
    println!("Gas: native={total_native_gas}, jit={total_jit_gas}");
    if total_mismatches > 0 {
        println!(
            "WARNING: {total_mismatches} gas mismatches across {} blocks",
            all_stats.len()
        );
    } else {
        println!(
            "All {total_txs} txs across {} blocks gas match OK",
            all_stats.len()
        );
    }
}
