//! Reproducible single-transaction benchmark from pre-tx snapshot.
//!
//! This tool:
//! 1) Replays the block up to `tx_index - 1` to build exact pre-tx snapshot.
//! 2) Benchmarks only the target tx on that snapshot (native vs JIT).
//! 3) Prints accessed accounts/storage slots and their values from journaled state.
//!
//! Usage:
//!   cargo run -p revmc-examples-runner --bin single_tx_bench --release -- \
//!     --block 38004930 --tx-index 185 --cache-dir /tmp/jit_cache

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
    primitives::{Address, B256, U256},
};
use revmc::OptimizationLevel;
use revmc_builtins as _;
use revmc_context::RawEvmCompilerFn;

use bin_common::{
    build_op_cfg, build_op_tx, compile_all_contracts_with_cache, BenchEvm, BinLoader, JitHandler, NativeHandler,
    OpCtx,
};

#[derive(Parser)]
#[command(name = "single_tx_bench")]
struct Args {
    #[arg(long, default_value = "/home/ubuntu/sipeng/bench_data")]
    dir: String,
    #[arg(long, default_value_t = 38004930)]
    block: u64,
    #[arg(long)]
    tx_index: usize,
    #[arg(long, default_value = "./jit_cache")]
    cache_dir: String,
    #[arg(long, default_value_t = 10)]
    rounds: usize,
    #[arg(long, default_value_t = 3)]
    warmup: usize,
    #[arg(long, default_value_t = 64)]
    max_slots: usize,
    /// Match bench scripts: skip deposit txs (0x7e) when building pre-tx snapshot.
    #[arg(long, action = clap::ArgAction::Set, default_value_t = true)]
    skip_deposits: bool,
}

#[derive(Clone)]
struct StorageAccess {
    addr: Address,
    slot: U256,
    original: U256,
    present: U256,
}

fn addr_hex(addr: Address) -> String {
    format!("0x{}", hex::encode(addr.as_slice()))
}

fn u256_hex(v: U256) -> String {
    format!("0x{}", hex::encode(v.to_be_bytes::<32>()))
}

fn snapshot_before_tx(
    loader: &BinLoader,
    tx_index: usize,
    skip_deposits: bool,
) -> Result<CacheDB<EmptyDB>, String> {
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
        if skip_deposits && tx_bin.tx_type == 0x7e {
            continue;
        }
        evm.0.ctx.tx = build_op_tx(tx_bin);
        let mut handler = NativeHandler;
        handler
            .run(&mut evm)
            .map_err(|e| format!("replay tx[{i}] failed: {e:?}"))?;
    }

    Ok(evm.0.ctx.journaled_state.database.clone())
}

fn collect_accesses(
    tx_bin: &bin_common::TxBin,
    snapshot: &CacheDB<EmptyDB>,
    chain_id: Option<u64>,
) -> (Vec<Address>, Vec<StorageAccess>) {
    let cfg = build_op_cfg(chain_id);
    let dummy_tx = OpTransaction::builder().build_fill();
    let mut evm: BenchEvm = {
        let ctx = OpCtx::<EmptyDB>::op()
            .with_cfg(cfg)
            .with_db(snapshot.clone())
            .with_tx(dummy_tx);
        op_revm::OpEvm::new(ctx, ())
    };
    evm.0.ctx.tx = build_op_tx(tx_bin);

    let mut handler = NativeHandler;
    let _ = handler.run(&mut evm);

    let mut accounts = Vec::new();
    let mut slots = Vec::new();
    for (addr, account) in &evm.0.ctx.journaled_state.state {
        accounts.push(*addr);
        for (slot, value) in &account.storage {
            slots.push(StorageAccess {
                addr: *addr,
                slot: *slot,
                original: value.original_value,
                present: value.present_value,
            });
        }
    }

    accounts.sort_by(|a, b| a.as_slice().cmp(b.as_slice()));
    slots.sort_by(|a, b| {
        let c = a.addr.as_slice().cmp(b.addr.as_slice());
        if c.is_eq() {
            a.slot.cmp(&b.slot)
        } else {
            c
        }
    });
    (accounts, slots)
}

fn run_once_native(
    tx_bin: &bin_common::TxBin,
    snapshot: &CacheDB<EmptyDB>,
    chain_id: Option<u64>,
) -> Duration {
    let cfg = build_op_cfg(chain_id);
    let dummy_tx = OpTransaction::builder().build_fill();
    let mut evm: BenchEvm = {
        let ctx = OpCtx::<EmptyDB>::op()
            .with_cfg(cfg)
            .with_db(snapshot.clone())
            .with_tx(dummy_tx);
        op_revm::OpEvm::new(ctx, ())
    };
    evm.0.ctx.tx = build_op_tx(tx_bin);

    let mut handler = NativeHandler;
    let t0 = Instant::now();
    let _ = handler.run(&mut evm);
    t0.elapsed()
}

fn run_once_jit(
    tx_bin: &bin_common::TxBin,
    snapshot: &CacheDB<EmptyDB>,
    chain_id: Option<u64>,
    functions: Arc<HashMap<B256, RawEvmCompilerFn>>,
) -> Duration {
    let cfg = build_op_cfg(chain_id);
    let dummy_tx = OpTransaction::builder().build_fill();
    let mut evm: BenchEvm = {
        let ctx = OpCtx::<EmptyDB>::op()
            .with_cfg(cfg)
            .with_db(snapshot.clone())
            .with_tx(dummy_tx);
        op_revm::OpEvm::new(ctx, ())
    };
    evm.0.ctx.tx = build_op_tx(tx_bin);

    let mut handler = JitHandler { functions };
    let t0 = Instant::now();
    let _ = handler.run(&mut evm);
    t0.elapsed()
}

fn run_target_with_prefix_native(
    loader: &BinLoader,
    tx_index: usize,
    skip_deposits: bool,
) -> Duration {
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

    let mut measured = Duration::ZERO;
    for (i, tx_bin) in loader.raw_txs().iter().enumerate() {
        if i > tx_index {
            break;
        }
        if skip_deposits && tx_bin.tx_type == 0x7e {
            continue;
        }
        evm.0.ctx.tx = build_op_tx(tx_bin);
        let mut handler = NativeHandler;
        if i == tx_index {
            let t0 = Instant::now();
            let _ = handler.run(&mut evm);
            measured = t0.elapsed();
            break;
        } else {
            let _ = handler.run(&mut evm);
        }
    }
    measured
}

fn run_target_with_prefix_jit(
    loader: &BinLoader,
    tx_index: usize,
    skip_deposits: bool,
    functions: Arc<HashMap<B256, RawEvmCompilerFn>>,
) -> Duration {
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

    let mut measured = Duration::ZERO;
    for (i, tx_bin) in loader.raw_txs().iter().enumerate() {
        if i > tx_index {
            break;
        }
        if skip_deposits && tx_bin.tx_type == 0x7e {
            continue;
        }
        evm.0.ctx.tx = build_op_tx(tx_bin);
        let mut handler = JitHandler {
            functions: functions.clone(),
        };
        if i == tx_index {
            let t0 = Instant::now();
            let _ = handler.run(&mut evm);
            measured = t0.elapsed();
            break;
        } else {
            let _ = handler.run(&mut evm);
        }
    }
    measured
}

fn run_full_block_and_pick_tx(
    loader: &BinLoader,
    tx_index: usize,
    skip_deposits: bool,
    functions: Option<&Arc<HashMap<B256, RawEvmCompilerFn>>>,
) -> Option<Duration> {
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

    let mut measured = None;
    for (i, tx_bin) in loader.raw_txs().iter().enumerate() {
        if skip_deposits && tx_bin.tx_type == 0x7e {
            continue;
        }
        evm.0.ctx.tx = build_op_tx(tx_bin);
        let t0 = Instant::now();
        if let Some(fns) = functions {
            let mut h = JitHandler {
                functions: fns.clone(),
            };
            let _ = h.run(&mut evm);
        } else {
            let mut h = NativeHandler;
            let _ = h.run(&mut evm);
        }
        if i == tx_index {
            measured = Some(t0.elapsed());
        }
    }
    measured
}

fn run_full_block_target_only_timer(
    loader: &BinLoader,
    tx_index: usize,
    skip_deposits: bool,
    functions: Option<&Arc<HashMap<B256, RawEvmCompilerFn>>>,
) -> Option<Duration> {
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

    let mut measured = None;
    for (i, tx_bin) in loader.raw_txs().iter().enumerate() {
        if skip_deposits && tx_bin.tx_type == 0x7e {
            continue;
        }
        evm.0.ctx.tx = build_op_tx(tx_bin);
        if let Some(fns) = functions {
            let mut h = JitHandler {
                functions: fns.clone(),
            };
            if i == tx_index {
                let t0 = Instant::now();
                let _ = h.run(&mut evm);
                measured = Some(t0.elapsed());
            } else {
                let _ = h.run(&mut evm);
            }
        } else {
            let mut h = NativeHandler;
            if i == tx_index {
                let t0 = Instant::now();
                let _ = h.run(&mut evm);
                measured = Some(t0.elapsed());
            } else {
                let _ = h.run(&mut evm);
            }
        }
    }
    measured
}

fn median_duration(mut v: Vec<Duration>) -> Duration {
    v.sort_unstable();
    v[v.len() / 2]
}

fn main() {
    let args = Args::parse();
    let loader = BinLoader::new(Path::new(&args.dir), args.block).expect("load block");
    assert!(
        args.tx_index < loader.tx_count(),
        "tx_index {} out of range [0, {})",
        args.tx_index,
        loader.tx_count()
    );

    let tx_bin = &loader.raw_txs()[args.tx_index];
    let chain_id = tx_bin.chain_id;

    println!("=== single_tx_bench ===");
    println!("block: {}", args.block);
    println!("tx_index: {}", args.tx_index);
    println!("tx_type: 0x{:02x}", tx_bin.tx_type);
    println!("caller: 0x{}", hex::encode(tx_bin.caller));
    println!(
        "to: {}",
        tx_bin
            .to
            .map(|a| format!("0x{}", hex::encode(a)))
            .unwrap_or_else(|| "CREATE".to_string())
    );
    println!("gas_limit: {}", tx_bin.gas_limit);
    println!("skip_deposits: {}", args.skip_deposits);
    println!("calldata_len: {}", tx_bin.data.len());
    if tx_bin.data.len() >= 4 {
        println!("selector: 0x{}", hex::encode(&tx_bin.data[..4]));
    } else {
        println!("selector: (none)");
    }

    // Contract metadata from pre-block snapshot (code hash is typically stable in block).
    if let Some(to) = tx_bin.to {
        let to_addr = Address::from_slice(&to);
        if let Some(acc) = loader.snapshot.accounts.get(&to_addr) {
            if let Some(info) = &acc.info {
                let code_hash = info.code_hash;
                let code_size = loader
                    .snapshot
                    .codes
                    .get(&code_hash)
                    .map(|bc| bc.original_byte_slice().len())
                    .unwrap_or(0);
                println!("to_code_hash: 0x{}", hex::encode(code_hash.as_slice()));
                println!("to_code_size: {}B", code_size);
            }
        }
    }

    println!("\n[1/3] Build pre-tx snapshot...");
    let pre_snapshot =
        snapshot_before_tx(&loader, args.tx_index, args.skip_deposits).expect("build pre-tx snapshot");

    println!("[2/3] Collect accessed accounts/slots from one native run...");
    let (accounts, slots) = collect_accesses(tx_bin, &pre_snapshot, chain_id);
    println!("accessed_accounts: {}", accounts.len());
    for addr in &accounts {
        println!("  account {}", addr_hex(*addr));
    }
    println!("accessed_storage_slots: {}", slots.len());
    for sa in slots.iter().take(args.max_slots) {
        let changed = if sa.original != sa.present { "changed" } else { "unchanged" };
        println!(
            "  {} slot={} original={} present={} {}",
            addr_hex(sa.addr),
            u256_hex(sa.slot),
            u256_hex(sa.original),
            u256_hex(sa.present),
            changed
        );
    }
    if slots.len() > args.max_slots {
        println!("  ... truncated {} more slots", slots.len() - args.max_slots);
    }

    println!("\n[3/3] Compile JIT and benchmark target tx...");
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
    println!("jit_functions: {}", compiled.functions.len());

    for _ in 0..args.warmup {
        let _ = run_once_native(tx_bin, &pre_snapshot, chain_id);
        let _ = run_once_jit(tx_bin, &pre_snapshot, chain_id, compiled.functions.clone());
    }

    let native_samples: Vec<Duration> = (0..args.rounds)
        .map(|_| run_once_native(tx_bin, &pre_snapshot, chain_id))
        .collect();
    let jit_samples: Vec<Duration> = (0..args.rounds)
        .map(|_| run_once_jit(tx_bin, &pre_snapshot, chain_id, compiled.functions.clone()))
        .collect();

    let native_med = median_duration(native_samples.clone());
    let jit_med = median_duration(jit_samples.clone());
    let speedup = native_med.as_secs_f64() / jit_med.as_secs_f64();
    let delta_us = (jit_med.as_secs_f64() - native_med.as_secs_f64()) * 1_000_000.0;

    println!("\n=== Isolated Snapshot Result ===");
    println!("native_median: {:.3}µs", native_med.as_secs_f64() * 1_000_000.0);
    println!("jit_median:    {:.3}µs", jit_med.as_secs_f64() * 1_000_000.0);
    println!("speedup:       {:.3}x (native/jit)", speedup);
    println!("delta:         {:+.3}µs (jit - native)", delta_us);

    if args.skip_deposits && tx_bin.tx_type == 0x7e {
        println!("\n=== Prefix-Aligned Result (full-block context) ===");
        println!("skipped: target tx is deposit (0x7e) and filtered by --skip-deposits");
        return;
    }

    // Prefix-aligned measurement: replay from block start each round and time only target tx.
    for _ in 0..args.warmup {
        let _ = run_target_with_prefix_native(&loader, args.tx_index, args.skip_deposits);
        let _ = run_target_with_prefix_jit(
            &loader,
            args.tx_index,
            args.skip_deposits,
            compiled.functions.clone(),
        );
    }
    let prefix_native_samples: Vec<Duration> = (0..args.rounds)
        .map(|_| run_target_with_prefix_native(&loader, args.tx_index, args.skip_deposits))
        .collect();
    let prefix_jit_samples: Vec<Duration> = (0..args.rounds)
        .map(|_| {
            run_target_with_prefix_jit(
                &loader,
                args.tx_index,
                args.skip_deposits,
                compiled.functions.clone(),
            )
        })
        .collect();
    let prefix_native_med = median_duration(prefix_native_samples);
    let prefix_jit_med = median_duration(prefix_jit_samples);
    let prefix_speedup = prefix_native_med.as_secs_f64() / prefix_jit_med.as_secs_f64();
    let prefix_delta_us =
        (prefix_jit_med.as_secs_f64() - prefix_native_med.as_secs_f64()) * 1_000_000.0;

    println!("\n=== Prefix-Aligned Result (full-block context) ===");
    println!(
        "native_median: {:.3}µs",
        prefix_native_med.as_secs_f64() * 1_000_000.0
    );
    println!(
        "jit_median:    {:.3}µs",
        prefix_jit_med.as_secs_f64() * 1_000_000.0
    );
    println!("speedup:       {:.3}x (native/jit)", prefix_speedup);
    println!("delta:         {:+.3}µs (jit - native)", prefix_delta_us);

    // Classify-style measurement: in each round, run full native block then full JIT block.
    for _ in 0..args.warmup {
        let _ = run_full_block_and_pick_tx(&loader, args.tx_index, args.skip_deposits, None);
        let _ = run_full_block_and_pick_tx(
            &loader,
            args.tx_index,
            args.skip_deposits,
            Some(&compiled.functions),
        );
    }
    let mut classify_native_samples = Vec::with_capacity(args.rounds);
    let mut classify_jit_samples = Vec::with_capacity(args.rounds);
    for _ in 0..args.rounds {
        if let Some(n) = run_full_block_and_pick_tx(&loader, args.tx_index, args.skip_deposits, None) {
            classify_native_samples.push(n);
        }
        if let Some(j) = run_full_block_and_pick_tx(
            &loader,
            args.tx_index,
            args.skip_deposits,
            Some(&compiled.functions),
        ) {
            classify_jit_samples.push(j);
        }
    }
    if !classify_native_samples.is_empty() && !classify_jit_samples.is_empty() {
        let cn = median_duration(classify_native_samples);
        let cj = median_duration(classify_jit_samples);
        let cs = cn.as_secs_f64() / cj.as_secs_f64();
        let cd = (cj.as_secs_f64() - cn.as_secs_f64()) * 1_000_000.0;
        println!("\n=== Classify-Style Full-Block Result ===");
        println!("native_median: {:.3}µs", cn.as_secs_f64() * 1_000_000.0);
        println!("jit_median:    {:.3}µs", cj.as_secs_f64() * 1_000_000.0);
        println!("speedup:       {:.3}x (native/jit)", cs);
        println!("delta:         {:+.3}µs (jit - native)", cd);
    }

    // Reverse-order control: full JIT block first, then full native block each round.
    for _ in 0..args.warmup {
        let _ = run_full_block_and_pick_tx(
            &loader,
            args.tx_index,
            args.skip_deposits,
            Some(&compiled.functions),
        );
        let _ = run_full_block_and_pick_tx(&loader, args.tx_index, args.skip_deposits, None);
    }
    let mut classify_rev_jit_samples = Vec::with_capacity(args.rounds);
    let mut classify_rev_native_samples = Vec::with_capacity(args.rounds);
    for _ in 0..args.rounds {
        if let Some(j) = run_full_block_and_pick_tx(
            &loader,
            args.tx_index,
            args.skip_deposits,
            Some(&compiled.functions),
        ) {
            classify_rev_jit_samples.push(j);
        }
        if let Some(n) = run_full_block_and_pick_tx(&loader, args.tx_index, args.skip_deposits, None) {
            classify_rev_native_samples.push(n);
        }
    }
    if !classify_rev_native_samples.is_empty() && !classify_rev_jit_samples.is_empty() {
        let rn = median_duration(classify_rev_native_samples);
        let rj = median_duration(classify_rev_jit_samples);
        let rs = rn.as_secs_f64() / rj.as_secs_f64();
        let rd = (rj.as_secs_f64() - rn.as_secs_f64()) * 1_000_000.0;
        println!("\n=== Classify-Style Full-Block Result (reversed order) ===");
        println!("native_median: {:.3}µs", rn.as_secs_f64() * 1_000_000.0);
        println!("jit_median:    {:.3}µs", rj.as_secs_f64() * 1_000_000.0);
        println!("speedup:       {:.3}x (native/jit)", rs);
        println!("delta:         {:+.3}µs (jit - native)", rd);
    }

    // Control: full-block replay with paired rounds, but only target tx is timed.
    for _ in 0..args.warmup {
        let _ = run_full_block_target_only_timer(&loader, args.tx_index, args.skip_deposits, None);
        let _ = run_full_block_target_only_timer(
            &loader,
            args.tx_index,
            args.skip_deposits,
            Some(&compiled.functions),
        );
    }
    let mut target_only_native = Vec::with_capacity(args.rounds);
    let mut target_only_jit = Vec::with_capacity(args.rounds);
    for _ in 0..args.rounds {
        if let Some(n) = run_full_block_target_only_timer(&loader, args.tx_index, args.skip_deposits, None) {
            target_only_native.push(n);
        }
        if let Some(j) = run_full_block_target_only_timer(
            &loader,
            args.tx_index,
            args.skip_deposits,
            Some(&compiled.functions),
        ) {
            target_only_jit.push(j);
        }
    }
    if !target_only_native.is_empty() && !target_only_jit.is_empty() {
        let n = median_duration(target_only_native);
        let j = median_duration(target_only_jit);
        let s = n.as_secs_f64() / j.as_secs_f64();
        let d = (j.as_secs_f64() - n.as_secs_f64()) * 1_000_000.0;
        println!("\n=== Full-Block Target-Only-Timer Result ===");
        println!("native_median: {:.3}µs", n.as_secs_f64() * 1_000_000.0);
        println!("jit_median:    {:.3}µs", j.as_secs_f64() * 1_000_000.0);
        println!("speedup:       {:.3}x (native/jit)", s);
        println!("delta:         {:+.3}µs (jit - native)", d);
    }
}
