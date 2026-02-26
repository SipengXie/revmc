//! Per-frame JIT vs Native profiling benchmark.
//!
//! Runs a single block from .bin files and instruments each call frame
//! to compare JIT and native interpreter execution times per frame.
//!
//! Usage:
//!   cargo run -p revmc-examples-runner --bin frame_bench --release -- --block 38004930
//!   cargo run -p revmc-examples-runner --bin frame_bench --release -- --block 38004930 --cache-dir /tmp/jit_cache

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
    bytecode::Bytecode,
    context_interface::{ContextTr, Transaction},
    database::EmptyDB,
    handler::{EvmTr, FrameResult, Handler, ItemOrResult},
    interpreter::InitialAndFloorGas,
    primitives::{Address, B256},
};
use revmc::OptimizationLevel;
use revmc_builtins as _;
use revmc_context::RawEvmCompilerFn;

use bin_common::{
    build_op_cfg, build_op_tx, compile_all_contracts, compile_all_contracts_with_cache,
    extract_gas, make_evm, run_jit_or_native, BenchError, BenchEvm, BinLoader, JitHandler,
    NativeHandler, OpCtx,
};

// ── Frame Profiling ─────────────────────────────────────────────────────────

#[derive(Clone)]
struct FrameRecord {
    bytecode_hash: B256,
    bytecode_size: usize,
    target_address: Address,
    total_duration: Duration,
    used_jit: bool,
    depth: usize,
}

/// Instrumented handler that records per-frame timing.
/// When `functions` is None, runs native interpreter.
/// When `functions` is Some, runs JIT where available, falls back to interpreter.
struct InstrumentedHandler {
    functions: Option<Arc<HashMap<B256, RawEvmCompilerFn>>>,
    bytecode_sizes: Arc<HashMap<B256, usize>>,
    frame_id_stack: Vec<usize>,
    frames: Vec<FrameRecord>,
}

impl InstrumentedHandler {
    fn new_native(bytecode_sizes: Arc<HashMap<B256, usize>>) -> Self {
        Self {
            functions: None,
            bytecode_sizes,
            frame_id_stack: Vec::new(),
            frames: Vec::new(),
        }
    }

    fn new_jit(
        functions: Arc<HashMap<B256, RawEvmCompilerFn>>,
        bytecode_sizes: Arc<HashMap<B256, usize>>,
    ) -> Self {
        Self {
            functions: Some(functions),
            bytecode_sizes,
            frame_id_stack: Vec::new(),
            frames: Vec::new(),
        }
    }

    fn into_frames(self) -> Vec<FrameRecord> {
        self.frames
    }

    /// Record a new frame entry from the current EVM frame stack.
    fn record_frame(&mut self, evm: &mut BenchEvm, depth: usize) {
        let frame = evm.0.frame_stack.get();
        let hash = frame.interpreter.bytecode.get_or_calculate_hash();
        let used_jit = self
            .functions
            .as_ref()
            .map_or(false, |f| f.contains_key(&hash));
        let size = self.bytecode_sizes.get(&hash).copied().unwrap_or(0);
        let idx = self.frames.len();
        self.frames.push(FrameRecord {
            bytecode_hash: hash,
            bytecode_size: size,
            target_address: frame.interpreter.input.target_address,
            total_duration: Duration::ZERO,
            used_jit,
            depth,
        });
        self.frame_id_stack.push(idx);
    }
}

impl Handler for InstrumentedHandler {
    type Evm = BenchEvm;
    type Error = BenchError;
    type HaltReason = OpHaltReason;

    fn run_exec_loop(
        &mut self,
        evm: &mut Self::Evm,
        first_frame_input: revm::interpreter::interpreter_action::FrameInit,
    ) -> Result<FrameResult, Self::Error> {
        let res = evm.frame_init(first_frame_input)?;
        if let ItemOrResult::Result(frame_result) = res {
            return Ok(frame_result);
        }

        self.record_frame(evm, 0);

        loop {
            let t0 = Instant::now();
            let call_or_result = if let Some(ref functions) = self.functions {
                run_jit_or_native(evm, functions)?
            } else {
                evm.frame_run()?
            };
            let elapsed = t0.elapsed();

            // Accumulate time for current top frame
            if let Some(&idx) = self.frame_id_stack.last() {
                self.frames[idx].total_duration += elapsed;
            }

            let (result, frame_completed) = match call_or_result {
                ItemOrResult::Item(init) => match evm.frame_init(init)? {
                    ItemOrResult::Item(_) => {
                        self.record_frame(evm, self.frame_id_stack.len());
                        continue;
                    }
                    ItemOrResult::Result(result) => (result, false),
                },
                ItemOrResult::Result(result) => (result, true),
            };

            if let Some(result) = evm.frame_return_result(result)? {
                if frame_completed {
                    self.frame_id_stack.pop();
                }
                return Ok(result);
            }
            if frame_completed {
                self.frame_id_stack.pop();
            }
        }
    }
}

// ── Block Benchmark ─────────────────────────────────────────────────────────

/// Run a full block with a given handler, return total execution duration.
fn bench_block<H: Handler<Evm = BenchEvm, Error = BenchError, HaltReason = OpHaltReason>>(
    loader: &BinLoader,
    chain_id: Option<u64>,
    handler: &mut H,
) -> Duration {
    let mut evm = make_evm(loader, chain_id);
    let mut total = Duration::ZERO;
    for tx_bin in loader.raw_txs() {
        if tx_bin.tx_type == 0x7e {
            continue;
        }
        evm.0.ctx.tx = build_op_tx(tx_bin);
        let t0 = Instant::now();
        let _ = handler.run(&mut evm);
        total += t0.elapsed();
    }
    total
}

const BENCH_ROUNDS: usize = 5;

/// Run bench_block N times, return median duration.
fn bench_block_median<H: Handler<Evm = BenchEvm, Error = BenchError, HaltReason = OpHaltReason>>(
    loader: &BinLoader,
    chain_id: Option<u64>,
    mut make_handler: impl FnMut() -> H,
) -> Duration {
    // Warmup
    bench_block(loader, chain_id, &mut make_handler());
    // Collect samples
    let mut samples: Vec<Duration> = (0..BENCH_ROUNDS)
        .map(|_| bench_block(loader, chain_id, &mut make_handler()))
        .collect();
    samples.sort();
    samples[BENCH_ROUNDS / 2]
}

// ── Timed Handlers (phase-level timing) ────────────────────────────────────

/// Native handler that times only run_exec_loop inside execution().
struct TimedNativeHandler {
    exec_loop_total: Duration,
}

impl TimedNativeHandler {
    fn new() -> Self {
        Self { exec_loop_total: Duration::ZERO }
    }
}

impl Handler for TimedNativeHandler {
    type Evm = BenchEvm;
    type Error = BenchError;
    type HaltReason = OpHaltReason;

    fn execution(
        &mut self,
        evm: &mut Self::Evm,
        init_and_floor_gas: &InitialAndFloorGas,
    ) -> Result<FrameResult, Self::Error> {
        let gas_limit = evm.ctx().tx().gas_limit() - init_and_floor_gas.initial_gas;
        let first_frame_input = self.first_frame_input(evm, gas_limit)?;
        let t0 = Instant::now();
        let mut frame_result = self.run_exec_loop(evm, first_frame_input)?;
        self.exec_loop_total += t0.elapsed();
        self.last_frame_result(evm, &mut frame_result)?;
        Ok(frame_result)
    }
}

/// JIT handler that times only run_exec_loop inside execution().
struct TimedJitHandler {
    functions: Arc<HashMap<B256, RawEvmCompilerFn>>,
    exec_loop_total: Duration,
}

impl TimedJitHandler {
    fn new(functions: Arc<HashMap<B256, RawEvmCompilerFn>>) -> Self {
        Self { functions, exec_loop_total: Duration::ZERO }
    }
}

impl Handler for TimedJitHandler {
    type Evm = BenchEvm;
    type Error = BenchError;
    type HaltReason = OpHaltReason;

    fn run_exec_loop(
        &mut self,
        evm: &mut Self::Evm,
        first_frame_input: revm::interpreter::interpreter_action::FrameInit,
    ) -> Result<FrameResult, Self::Error> {
        let res = evm.frame_init(first_frame_input)?;
        if let ItemOrResult::Result(frame_result) = res {
            return Ok(frame_result);
        }
        loop {
            let call_or_result = run_jit_or_native(evm, &self.functions)?;
            let result = match call_or_result {
                ItemOrResult::Item(init) => match evm.frame_init(init)? {
                    ItemOrResult::Item(_) => continue,
                    ItemOrResult::Result(result) => result,
                },
                ItemOrResult::Result(result) => result,
            };
            if let Some(result) = evm.frame_return_result(result)? {
                return Ok(result);
            }
        }
    }

    fn execution(
        &mut self,
        evm: &mut Self::Evm,
        init_and_floor_gas: &InitialAndFloorGas,
    ) -> Result<FrameResult, Self::Error> {
        let gas_limit = evm.ctx().tx().gas_limit() - init_and_floor_gas.initial_gas;
        let first_frame_input = self.first_frame_input(evm, gas_limit)?;
        let t0 = Instant::now();
        let mut frame_result = self.run_exec_loop(evm, first_frame_input)?;
        self.exec_loop_total += t0.elapsed();
        self.last_frame_result(evm, &mut frame_result)?;
        Ok(frame_result)
    }
}

/// Run a full block with a timed handler, return (total_wall_time, exec_loop_time).
fn bench_block_timed<H: Handler<Evm = BenchEvm, Error = BenchError, HaltReason = OpHaltReason>>(
    loader: &BinLoader,
    chain_id: Option<u64>,
    handler: &mut H,
    get_exec_loop: impl Fn(&H) -> Duration,
) -> (Duration, Duration) {
    let mut evm = make_evm(loader, chain_id);
    let mut total = Duration::ZERO;
    for tx_bin in loader.raw_txs() {
        if tx_bin.tx_type == 0x7e {
            continue;
        }
        evm.0.ctx.tx = build_op_tx(tx_bin);
        let t0 = Instant::now();
        let _ = handler.run(&mut evm);
        total += t0.elapsed();
    }
    (total, get_exec_loop(handler))
}

/// Run timed benchmark with warmup + median, return median (total, exec_loop).
fn bench_block_timed_median<H: Handler<Evm = BenchEvm, Error = BenchError, HaltReason = OpHaltReason>>(
    loader: &BinLoader,
    chain_id: Option<u64>,
    mut make_handler: impl FnMut() -> H,
    get_exec_loop: impl Fn(&H) -> Duration,
) -> (Duration, Duration) {
    // Warmup
    bench_block_timed(loader, chain_id, &mut make_handler(), &get_exec_loop);
    // Collect samples
    let mut samples: Vec<(Duration, Duration)> = (0..BENCH_ROUNDS)
        .map(|_| bench_block_timed(loader, chain_id, &mut make_handler(), &get_exec_loop))
        .collect();
    samples.sort_by_key(|s| s.0);
    samples[BENCH_ROUNDS / 2]
}

// ── Frame Comparison ────────────────────────────────────────────────────────

struct FrameComparison {
    bytecode_hash: B256,
    bytecode_size: usize,
    target_address: Address,
    depth: usize,
    native_dur: Duration,
    jit_dur: Duration,
    has_jit: bool,
}

fn compare_frames(
    native_frames: &[FrameRecord],
    jit_frames: &[FrameRecord],
) -> Vec<FrameComparison> {
    native_frames
        .iter()
        .zip(jit_frames)
        .map(|(nf, jf)| FrameComparison {
            bytecode_hash: nf.bytecode_hash,
            bytecode_size: nf.bytecode_size,
            target_address: nf.target_address,
            depth: nf.depth,
            native_dur: nf.total_duration,
            jit_dur: jf.total_duration,
            has_jit: jf.used_jit,
        })
        .collect()
}

// ── Output ──────────────────────────────────────────────────────────────────

fn short_hash(hash: &B256) -> String {
    let h = hex::encode(hash);
    format!("{}..{}", &h[..6], &h[60..])
}

fn short_addr(addr: &Address) -> String {
    let h = hex::encode(addr);
    format!("0x{}..{}", &h[..4], &h[36..])
}

fn speedup(native: Duration, jit: Duration) -> f64 {
    let jit_secs = jit.as_secs_f64();
    if jit_secs > 0.0 {
        native.as_secs_f64() / jit_secs
    } else {
        f64::INFINITY
    }
}

fn jit_label(has_jit: bool) -> &'static str {
    if has_jit { "yes" } else { "no" }
}

fn print_tx_frames(tx_index: usize, gas: u64, comparisons: &[FrameComparison]) {
    if comparisons.is_empty() {
        return;
    }
    println!(
        "\n--- Tx {tx_index} (gas={gas}) | {n} frames ---",
        n = comparisons.len()
    );
    println!(
        "  {:>4} {:>5} {:>14} {:>12} {:>7} {:>12} {:>12} {:>8} {:>4}",
        "#", "Depth", "Contract", "Hash", "Size", "Native(us)", "JIT(us)", "Speedup", "JIT?"
    );
    println!(
        "  {:-<4} {:-<5} {:-<14} {:-<12} {:-<7} {:-<12} {:-<12} {:-<8} {:-<4}",
        "", "", "", "", "", "", "", "", ""
    );
    for (i, c) in comparisons.iter().enumerate() {
        let native_us = c.native_dur.as_secs_f64() * 1_000_000.0;
        let jit_us = c.jit_dur.as_secs_f64() * 1_000_000.0;
        println!(
            "  {:>4} {:>5} {:>14} {:>12} {:>7} {:>12.1} {:>12.1} {:>7.2}x {:>4}",
            i,
            c.depth,
            short_addr(&c.target_address),
            short_hash(&c.bytecode_hash),
            c.bytecode_size,
            native_us,
            jit_us,
            speedup(c.native_dur, c.jit_dur),
            jit_label(c.has_jit),
        );
    }
}

struct ContractAggregate {
    bytecode_hash: B256,
    bytecode_size: usize,
    invocations: usize,
    native_total: Duration,
    jit_total: Duration,
    has_jit: bool,
}

fn aggregate_by_contract(all_comparisons: &[FrameComparison]) -> Vec<ContractAggregate> {
    let mut map: HashMap<B256, ContractAggregate> = HashMap::new();
    for c in all_comparisons {
        let entry = map.entry(c.bytecode_hash).or_insert_with(|| ContractAggregate {
            bytecode_hash: c.bytecode_hash,
            bytecode_size: c.bytecode_size,
            invocations: 0,
            native_total: Duration::ZERO,
            jit_total: Duration::ZERO,
            has_jit: c.has_jit,
        });
        entry.invocations += 1;
        entry.native_total += c.native_dur;
        entry.jit_total += c.jit_dur;
    }
    let mut aggregates: Vec<_> = map.into_values().collect();
    aggregates.sort_by(|a, b| b.native_total.cmp(&a.native_total));
    aggregates
}

fn print_contract_summary(aggregates: &[ContractAggregate]) {
    println!("\n========== Per-Contract Summary ==========");
    println!(
        "  {:>12} {:>7} {:>6} {:>12} {:>12} {:>8} {:>4}",
        "Hash", "Size", "Calls", "Native(us)", "JIT(us)", "Speedup", "JIT?"
    );
    println!(
        "  {:-<12} {:-<7} {:-<6} {:-<12} {:-<12} {:-<8} {:-<4}",
        "", "", "", "", "", "", ""
    );
    for agg in aggregates {
        let native_us = agg.native_total.as_secs_f64() * 1_000_000.0;
        let jit_us = agg.jit_total.as_secs_f64() * 1_000_000.0;
        println!(
            "  {:>12} {:>7} {:>6} {:>12.1} {:>12.1} {:>7.2}x {:>4}",
            short_hash(&agg.bytecode_hash),
            agg.bytecode_size,
            agg.invocations,
            native_us,
            jit_us,
            speedup(agg.native_total, agg.jit_total),
            jit_label(agg.has_jit),
        );
    }
}

fn print_block_summary(all_comparisons: &[FrameComparison]) {
    let total_frames = all_comparisons.len();
    let total_native: Duration = all_comparisons.iter().map(|c| c.native_dur).sum();
    let total_jit: Duration = all_comparisons.iter().map(|c| c.jit_dur).sum();
    let jit_faster = all_comparisons
        .iter()
        .filter(|c| c.jit_dur < c.native_dur)
        .count();
    let native_faster = all_comparisons
        .iter()
        .filter(|c| c.native_dur < c.jit_dur)
        .count();
    let equal = total_frames - jit_faster - native_faster;
    let overall_speedup = speedup(total_native, total_jit);

    // JIT-only: frames where JIT was available
    let jit_native_total: Duration = all_comparisons
        .iter()
        .filter(|c| c.has_jit)
        .map(|c| c.native_dur)
        .sum();
    let jit_jit_total: Duration = all_comparisons
        .iter()
        .filter(|c| c.has_jit)
        .map(|c| c.jit_dur)
        .sum();
    let jit_frame_count = all_comparisons.iter().filter(|c| c.has_jit).count();
    let jit_only_speedup = if jit_frame_count > 0 {
        speedup(jit_native_total, jit_jit_total)
    } else {
        1.0
    };

    let pct = |n: usize| n as f64 / total_frames as f64 * 100.0;

    println!("\n========== Block Summary ==========");
    println!("Total frames: {total_frames}");
    println!(
        "Native total: {:.2}ms",
        total_native.as_secs_f64() * 1000.0
    );
    println!("JIT total:    {:.2}ms", total_jit.as_secs_f64() * 1000.0);
    println!("Overall speedup: {overall_speedup:.2}x");
    println!(
        "JIT-compiled frames only ({jit_frame_count}/{total_frames}): speedup {jit_only_speedup:.2}x",
    );
    println!(
        "JIT faster:    {jit_faster}/{total_frames} ({:.1}%)",
        pct(jit_faster)
    );
    println!(
        "Native faster: {native_faster}/{total_frames} ({:.1}%)",
        pct(native_faster)
    );
    if equal > 0 {
        println!(
            "Equal:         {equal}/{total_frames} ({:.1}%)",
            pct(equal)
        );
    }
}

// ── CLI + Main ──────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "frame_bench",
    about = "Per-frame JIT vs Native profiling for a single block"
)]
struct Args {
    /// Path to bench_data directory
    #[arg(long, default_value = "/home/ubuntu/sipeng/bench_data")]
    dir: String,

    /// Block number to profile
    #[arg(long)]
    block: u64,

    /// Persistent AOT cache directory
    #[arg(long)]
    cache_dir: Option<String>,

    /// Show per-transaction per-frame details (default: only summary)
    #[arg(long, default_value_t = false)]
    verbose: bool,
}


fn main() {
    let args = Args::parse();
    let bench_dir = Path::new(&args.dir);

    // Phase 1: Load block
    eprintln!("=== Loading block {} ===", args.block);
    let loader = match BinLoader::new(bench_dir, args.block) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to load block {}: {e}", args.block);
            std::process::exit(1);
        }
    };
    eprintln!(
        "  {} txs, {} accounts, {} codes",
        loader.tx_count(),
        loader.account_count(),
        loader.code_count()
    );

    // Phase 2: Compile
    eprintln!("\n=== Compiling contracts ===");
    let code_values: HashMap<B256, Bytecode> = loader
        .code_values()
        .iter()
        .map(|(h, bc)| (*h, bc.clone()))
        .collect();

    let compile_start = Instant::now();
    let compiled = if let Some(ref cache_dir) = args.cache_dir {
        compile_all_contracts_with_cache(
            &code_values,
            OptimizationLevel::Aggressive,
            Path::new(cache_dir),
        )
    } else {
        compile_all_contracts(&code_values)
    };
    eprintln!(
        "  Compilation time: {:.2}s\n",
        compile_start.elapsed().as_secs_f64()
    );

    // Build bytecode size map
    let bytecode_sizes: Arc<HashMap<B256, usize>> = Arc::new(
        code_values
            .iter()
            .map(|(h, bc)| (*h, bc.original_byte_slice().len()))
            .collect(),
    );

    // Phase 3: Warmup + instrumented per-frame profiling
    let chain_id = loader.raw_txs().first().and_then(|tx| tx.chain_id);

    // Warmup: run both native and JIT once to load all code into icache
    eprintln!("=== Warmup: loading JIT code into icache ===");
    bench_block(&loader, chain_id, &mut NativeHandler);
    bench_block(&loader, chain_id, &mut JitHandler {
        functions: compiled.functions.clone(),
    });

    eprintln!("=== Profiling: Native vs JIT per frame (warm) ===");
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

    let mut all_comparisons: Vec<FrameComparison> = Vec::new();
    let mut gas_mismatches = 0usize;
    let mut skipped = 0usize;
    let mut native_wall = Duration::ZERO;
    let mut jit_wall = Duration::ZERO;

    println!(
        "=== Block {} | {} txs ===",
        loader.block_number(),
        loader.tx_count()
    );

    for (i, tx_bin) in loader.raw_txs().iter().enumerate() {
        if tx_bin.tx_type == 0x7e {
            skipped += 1;
            continue;
        }
        let op_tx = build_op_tx(tx_bin);

        // Native run
        native_evm.0.ctx.tx = op_tx.clone();
        let mut native_handler = InstrumentedHandler::new_native(bytecode_sizes.clone());
        let t_native = Instant::now();
        let native_result = native_handler.run(&mut native_evm);
        native_wall += t_native.elapsed();
        let (_, native_gas) = match &native_result {
            Ok(r) => extract_gas(r),
            Err(e) => {
                eprintln!("  native tx[{i}] error: {e:?}");
                skipped += 1;
                continue;
            }
        };

        // JIT run
        jit_evm.0.ctx.tx = op_tx;
        let mut jit_handler =
            InstrumentedHandler::new_jit(compiled.functions.clone(), bytecode_sizes.clone());
        let t_jit = Instant::now();
        let jit_result = jit_handler.run(&mut jit_evm);
        jit_wall += t_jit.elapsed();
        let (_, jit_gas) = match &jit_result {
            Ok(r) => extract_gas(r),
            Err(e) => {
                eprintln!("  jit tx[{i}] error: {e:?}");
                skipped += 1;
                continue;
            }
        };

        if native_gas != jit_gas {
            eprintln!("  GAS MISMATCH tx[{i}]: native={native_gas} jit={jit_gas}");
            gas_mismatches += 1;
        }

        let native_frames = native_handler.into_frames();
        let jit_frames = jit_handler.into_frames();

        if native_frames.len() != jit_frames.len() {
            eprintln!(
                "  FRAME COUNT MISMATCH tx[{i}]: native={} jit={}",
                native_frames.len(),
                jit_frames.len()
            );
        }

        let comparisons = compare_frames(&native_frames, &jit_frames);
        if args.verbose {
            print_tx_frames(i, native_gas, &comparisons);
        }
        all_comparisons.extend(comparisons);
    }

    // Phase 4: Output (Native vs Full-JIT)
    let aggregates = aggregate_by_contract(&all_comparisons);
    print_contract_summary(&aggregates);
    print_block_summary(&all_comparisons);

    // Diagnostic: compare frame-accumulated time vs wall-clock handler.run() time
    let total_native_frames: Duration = all_comparisons.iter().map(|c| c.native_dur).sum();
    let total_jit_frames: Duration = all_comparisons.iter().map(|c| c.jit_dur).sum();
    println!("\n========== Timing Diagnostic ==========");
    println!(
        "  Native: frames={:.2}ms  wall={:.2}ms  overhead={:.2}ms",
        total_native_frames.as_secs_f64() * 1000.0,
        native_wall.as_secs_f64() * 1000.0,
        (native_wall - total_native_frames).as_secs_f64() * 1000.0,
    );
    println!(
        "  JIT:    frames={:.2}ms  wall={:.2}ms  overhead={:.2}ms",
        total_jit_frames.as_secs_f64() * 1000.0,
        jit_wall.as_secs_f64() * 1000.0,
        (jit_wall - total_jit_frames).as_secs_f64() * 1000.0,
    );

    if gas_mismatches > 0 {
        println!("\nWARNING: {gas_mismatches} gas mismatches detected!");
    }
    if skipped > 0 {
        println!("Skipped: {skipped} transactions (deposits/errors)");
    }

    // Phase 5: Clean benchmark with invocation-count heuristic
    let all_functions = compiled.functions.clone();
    let jit_contract_count = aggregates.iter().filter(|agg| agg.has_jit).count();

    // Build whitelists by invocation count threshold
    let thresholds: &[usize] = &[1, 2, 5, 10, 20, 50];
    let whitelists: Vec<(usize, Arc<HashMap<B256, RawEvmCompilerFn>>)> = thresholds
        .iter()
        .map(|&min_calls| {
            let wl: HashMap<B256, RawEvmCompilerFn> = aggregates
                .iter()
                .filter(|agg| agg.has_jit && agg.invocations >= min_calls)
                .filter_map(|agg| {
                    compiled
                        .functions
                        .get(&agg.bytecode_hash)
                        .map(|&f| (agg.bytecode_hash, f))
                })
                .collect();
            (min_calls, Arc::new(wl))
        })
        .collect();

    // Warm-instrumented whitelist: contracts where JIT speedup > 1.0 in warm per-frame data
    let warm_whitelist: Arc<HashMap<B256, RawEvmCompilerFn>> = Arc::new(
        aggregates
            .iter()
            .filter(|agg| agg.has_jit && agg.jit_total < agg.native_total)
            .filter_map(|agg| {
                compiled
                    .functions
                    .get(&agg.bytecode_hash)
                    .map(|&f| (agg.bytecode_hash, f))
            })
            .collect(),
    );

    println!("\n========== Whitelist Strategies ==========");
    println!(
        "  Warm-instrumented (speedup>1.0): {}/{} contracts",
        warm_whitelist.len(),
        jit_contract_count,
    );
    for (min_calls, wl) in &whitelists {
        let total_invocations: usize = aggregates
            .iter()
            .filter(|agg| wl.contains_key(&agg.bytecode_hash))
            .map(|agg| agg.invocations)
            .sum();
        println!(
            "  calls>={:<3} → {}/{} contracts, {} total frame invocations",
            min_calls,
            wl.len(),
            jit_contract_count,
            total_invocations,
        );
    }

    // Benchmark: native, full JIT, warm-instrumented, and each threshold
    eprintln!(
        "\n=== Clean Benchmark (1 warmup + {} rounds, median) ===",
        BENCH_ROUNDS
    );

    let native_dur = bench_block_median(&loader, chain_id, || NativeHandler);

    let full_jit_dur = bench_block_median(&loader, chain_id, {
        let fns = all_functions.clone();
        move || JitHandler {
            functions: fns.clone(),
        }
    });

    let warm_selective_dur = bench_block_median(&loader, chain_id, {
        let fns = warm_whitelist.clone();
        move || JitHandler {
            functions: fns.clone(),
        }
    });

    let mut threshold_results: Vec<(usize, usize, Duration)> = Vec::new();
    for (min_calls, wl) in &whitelists {
        let dur = bench_block_median(&loader, chain_id, {
            let fns = wl.clone();
            move || JitHandler {
                functions: fns.clone(),
            }
        });
        threshold_results.push((*min_calls, wl.len(), dur));
    }

    println!(
        "\n========== Benchmark Results (median of {} runs) ==========",
        BENCH_ROUNDS
    );
    println!(
        "  {:>18} {:>10} {:>10} {:>10}",
        "Mode", "Contracts", "Time(ms)", "Speedup"
    );
    println!(
        "  {:->18} {:->10} {:->10} {:->10}",
        "", "", "", ""
    );
    println!(
        "  {:>18} {:>10} {:>10.2} {:>9.2}x",
        "Native", "-", native_dur.as_secs_f64() * 1000.0, 1.0,
    );
    println!(
        "  {:>18} {:>10} {:>10.2} {:>9.2}x",
        "Full JIT",
        jit_contract_count,
        full_jit_dur.as_secs_f64() * 1000.0,
        speedup(native_dur, full_jit_dur),
    );
    println!(
        "  {:>18} {:>10} {:>10.2} {:>9.2}x",
        "warm-selective",
        warm_whitelist.len(),
        warm_selective_dur.as_secs_f64() * 1000.0,
        speedup(native_dur, warm_selective_dur),
    );
    for (min_calls, n_contracts, dur) in &threshold_results {
        println!(
            "  {:>18} {:>10} {:>10.2} {:>9.2}x",
            format!("calls>={}", min_calls),
            n_contracts,
            dur.as_secs_f64() * 1000.0,
            speedup(native_dur, *dur),
        );
    }

    // Phase 6: Phase-level timing (exec_loop vs non-exec overhead)
    eprintln!(
        "\n=== Phase-Level Timing (1 warmup + {} rounds, median) ===",
        BENCH_ROUNDS
    );

    let (native_total, native_exec) = bench_block_timed_median(
        &loader,
        chain_id,
        || TimedNativeHandler::new(),
        |h| h.exec_loop_total,
    );

    let (jit_total, jit_exec) = bench_block_timed_median(
        &loader,
        chain_id,
        {
            let fns = all_functions.clone();
            move || TimedJitHandler::new(fns.clone())
        },
        |h| h.exec_loop_total,
    );

    let native_non_exec = native_total - native_exec;
    let jit_non_exec = jit_total - jit_exec;
    let native_exec_pct = native_exec.as_secs_f64() / native_total.as_secs_f64() * 100.0;
    let jit_exec_pct = jit_exec.as_secs_f64() / jit_total.as_secs_f64() * 100.0;
    let exec_speedup = speedup(native_exec, jit_exec);
    let non_exec_ratio = native_non_exec.as_secs_f64() / native_total.as_secs_f64();

    // Amdahl prediction: S_total = 1 / ((1 - f_exec) + f_exec / S_exec)
    let f_exec = native_exec.as_secs_f64() / native_total.as_secs_f64();
    let amdahl_predicted = 1.0 / ((1.0 - f_exec) + f_exec / exec_speedup);

    println!("\n========== Phase-Level Breakdown ==========");
    println!(
        "  Native:  total={:.2}ms  exec_loop={:.2}ms ({:.1}%)  non_exec={:.2}ms ({:.1}%)",
        native_total.as_secs_f64() * 1000.0,
        native_exec.as_secs_f64() * 1000.0,
        native_exec_pct,
        native_non_exec.as_secs_f64() * 1000.0,
        100.0 - native_exec_pct,
    );
    println!(
        "  JIT:     total={:.2}ms  exec_loop={:.2}ms ({:.1}%)  non_exec={:.2}ms ({:.1}%)",
        jit_total.as_secs_f64() * 1000.0,
        jit_exec.as_secs_f64() * 1000.0,
        jit_exec_pct,
        jit_non_exec.as_secs_f64() * 1000.0,
        100.0 - jit_exec_pct,
    );
    println!("\n  exec_loop speedup: {exec_speedup:.2}x");
    println!("  non_exec overhead: {:.1}% of native total", non_exec_ratio * 100.0);
    println!("  Amdahl predicted e2e speedup: {amdahl_predicted:.2}x");
    println!(
        "  Actual e2e speedup (Phase 5): {:.2}x",
        speedup(native_dur, full_jit_dur),
    );
}
