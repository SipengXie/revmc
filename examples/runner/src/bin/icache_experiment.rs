//! L1i cache thrashing experiment for JIT vs native.
//!
//! Runs two modes controlled by --mode:
//!   isolated: clone pre-snapshot, run ONLY the target tx N times
//!   fullblock: replay full block from scratch each round, timing target tx
//!
//! Run with `perf stat -e L1-icache-load-misses,instructions` to compare.

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
    database::{CacheDB, EmptyDB},
    handler::Handler,
    primitives::B256,
};
use revmc::OptimizationLevel;
use revmc_builtins as _;
use revmc_context::RawEvmCompilerFn;

use bin_common::{
    build_op_cfg, build_op_tx, compile_all_contracts_with_cache, BenchEvm, BinLoader, JitHandler,
    NativeHandler, OpCtx,
};

#[derive(Parser)]
#[command(name = "icache_experiment")]
struct Args {
    #[arg(long, default_value = "/home/ubuntu/sipeng/bench_data")]
    dir: String,
    #[arg(long, default_value_t = 38004930)]
    block: u64,
    #[arg(long)]
    tx_index: usize,
    #[arg(long, default_value = "/tmp/jit_cache")]
    cache_dir: String,
    #[arg(long, default_value_t = 50)]
    rounds: usize,
    /// Mode: isolated_jit, isolated_native, fullblock_jit, fullblock_native, prefix_jit, prefix_native
    #[arg(long)]
    mode: String,
}

fn snapshot_before_tx(loader: &BinLoader, tx_index: usize) -> CacheDB<EmptyDB> {
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
    for (i, tx_bin) in loader.raw_txs().iter().enumerate() {
        if i == tx_index {
            break;
        }
        if tx_bin.tx_type == 0x7e {
            continue;
        }
        evm.0.ctx.tx = build_op_tx(tx_bin);
        let mut handler = NativeHandler;
        let _ = handler.run(&mut evm);
    }
    evm.0.ctx.journaled_state.database.clone()
}

fn main() {
    let args = Args::parse();
    let loader = BinLoader::new(Path::new(&args.dir), args.block).expect("load block");
    let tx_bin = &loader.raw_txs()[args.tx_index];
    let chain_id = tx_bin.chain_id;

    // Compile JIT
    let all_codes: HashMap<B256, _> = loader
        .code_values()
        .iter()
        .map(|(h, bc)| (*h, bc.clone()))
        .collect();
    let compiled = compile_all_contracts_with_cache(
        &all_codes,
        OptimizationLevel::Aggressive,
        Path::new(&args.cache_dir),
    );
    eprintln!("jit_functions: {}", compiled.functions.len());

    let pre_snapshot = snapshot_before_tx(&loader, args.tx_index);
    eprintln!("pre-snapshot built, mode={}, rounds={}", args.mode, args.rounds);

    // Signal: measurement starts here (for perf analysis)
    let wall_start = Instant::now();
    let mut total = Duration::ZERO;

    match args.mode.as_str() {
        "isolated_jit" => {
            for _ in 0..args.rounds {
                let cfg = build_op_cfg(chain_id);
                let dummy_tx = OpTransaction::builder().build_fill();
                let mut evm: BenchEvm = {
                    let ctx = OpCtx::<EmptyDB>::op()
                        .with_cfg(cfg)
                        .with_db(pre_snapshot.clone())
                        .with_tx(dummy_tx);
                    op_revm::OpEvm::new(ctx, ())
                };
                evm.0.ctx.tx = build_op_tx(tx_bin);
                let mut handler = JitHandler {
                    functions: compiled.functions.clone(),
                };
                let t0 = Instant::now();
                let _ = handler.run(&mut evm);
                total += t0.elapsed();
            }
        }
        "isolated_native" => {
            for _ in 0..args.rounds {
                let cfg = build_op_cfg(chain_id);
                let dummy_tx = OpTransaction::builder().build_fill();
                let mut evm: BenchEvm = {
                    let ctx = OpCtx::<EmptyDB>::op()
                        .with_cfg(cfg)
                        .with_db(pre_snapshot.clone())
                        .with_tx(dummy_tx);
                    op_revm::OpEvm::new(ctx, ())
                };
                evm.0.ctx.tx = build_op_tx(tx_bin);
                let mut handler = NativeHandler;
                let t0 = Instant::now();
                let _ = handler.run(&mut evm);
                total += t0.elapsed();
            }
        }
        "prefix_jit" => {
            for _ in 0..args.rounds {
                let cfg = build_op_cfg(chain_id);
                let dummy_tx = OpTransaction::builder().build_fill();
                let mut evm: BenchEvm = {
                    let ctx = OpCtx::<EmptyDB>::op()
                        .with_cfg(cfg)
                        .with_db(loader.build_cache_db())
                        .with_tx(dummy_tx);
                    op_revm::OpEvm::new(ctx, ())
                };
                for (i, tb) in loader.raw_txs().iter().enumerate() {
                    if i > args.tx_index {
                        break;
                    }
                    if tb.tx_type == 0x7e {
                        continue;
                    }
                    evm.0.ctx.tx = build_op_tx(tb);
                    let mut handler = JitHandler {
                        functions: compiled.functions.clone(),
                    };
                    if i == args.tx_index {
                        let t0 = Instant::now();
                        let _ = handler.run(&mut evm);
                        total += t0.elapsed();
                    } else {
                        let _ = handler.run(&mut evm);
                    }
                }
            }
        }
        "prefix_native" => {
            for _ in 0..args.rounds {
                let cfg = build_op_cfg(chain_id);
                let dummy_tx = OpTransaction::builder().build_fill();
                let mut evm: BenchEvm = {
                    let ctx = OpCtx::<EmptyDB>::op()
                        .with_cfg(cfg)
                        .with_db(loader.build_cache_db())
                        .with_tx(dummy_tx);
                    op_revm::OpEvm::new(ctx, ())
                };
                for (i, tb) in loader.raw_txs().iter().enumerate() {
                    if i > args.tx_index {
                        break;
                    }
                    if tb.tx_type == 0x7e {
                        continue;
                    }
                    evm.0.ctx.tx = build_op_tx(tb);
                    let mut handler = NativeHandler;
                    if i == args.tx_index {
                        let t0 = Instant::now();
                        let _ = handler.run(&mut evm);
                        total += t0.elapsed();
                    } else {
                        let _ = handler.run(&mut evm);
                    }
                }
            }
        }
        "fullblock_jit" => {
            for _ in 0..args.rounds {
                let cfg = build_op_cfg(chain_id);
                let dummy_tx = OpTransaction::builder().build_fill();
                let mut evm: BenchEvm = {
                    let ctx = OpCtx::<EmptyDB>::op()
                        .with_cfg(cfg)
                        .with_db(loader.build_cache_db())
                        .with_tx(dummy_tx);
                    op_revm::OpEvm::new(ctx, ())
                };
                for (i, tb) in loader.raw_txs().iter().enumerate() {
                    if tb.tx_type == 0x7e {
                        continue;
                    }
                    evm.0.ctx.tx = build_op_tx(tb);
                    let mut handler = JitHandler {
                        functions: compiled.functions.clone(),
                    };
                    if i == args.tx_index {
                        let t0 = Instant::now();
                        let _ = handler.run(&mut evm);
                        total += t0.elapsed();
                    } else {
                        let _ = handler.run(&mut evm);
                    }
                }
            }
        }
        "fullblock_native" => {
            for _ in 0..args.rounds {
                let cfg = build_op_cfg(chain_id);
                let dummy_tx = OpTransaction::builder().build_fill();
                let mut evm: BenchEvm = {
                    let ctx = OpCtx::<EmptyDB>::op()
                        .with_cfg(cfg)
                        .with_db(loader.build_cache_db())
                        .with_tx(dummy_tx);
                    op_revm::OpEvm::new(ctx, ())
                };
                for (i, tb) in loader.raw_txs().iter().enumerate() {
                    if tb.tx_type == 0x7e {
                        continue;
                    }
                    evm.0.ctx.tx = build_op_tx(tb);
                    let mut handler = NativeHandler;
                    if i == args.tx_index {
                        let t0 = Instant::now();
                        let _ = handler.run(&mut evm);
                        total += t0.elapsed();
                    } else {
                        let _ = handler.run(&mut evm);
                    }
                }
            }
        }
        other => {
            eprintln!("unknown mode: {other}");
            std::process::exit(1);
        }
    }

    let wall = wall_start.elapsed();
    let avg_us = total.as_secs_f64() / args.rounds as f64 * 1_000_000.0;
    println!(
        "mode={} rounds={} total_target={:.1}µs avg_target={:.1}µs wall={:.1}ms",
        args.mode,
        args.rounds,
        total.as_secs_f64() * 1_000_000.0,
        avg_us,
        wall.as_secs_f64() * 1000.0,
    );
}
