//! Deep-dive analysis of the CREATE tx (tx_idx=130) that is 0.66x with JIT.
//!
//! Answers:
//!   1. Is the initcode actually JIT-compiled?
//!   2. What is the initcode doing? (constructor opcode breakdown)
//!   3. Where does the 82µs JIT overhead come from?
//!
//! Usage:
//!   cargo run -p revmc-examples-runner --bin analyze_create_tx --release

#[allow(dead_code)]
#[path = "../bin_common.rs"]
mod bin_common;

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use clap::Parser;
use op_revm::{transaction::OpTransaction, DefaultOp};
use revm::{
    database::EmptyDB,
    handler::{EvmTr, FrameResult, Handler, ItemOrResult},
    interpreter::interpreter_action::FrameInit,
    primitives::{keccak256, B256},
};
use revmc::OptimizationLevel;
use revmc_builtins as _;

use bin_common::{
    build_op_cfg, build_op_tx, compile_all_contracts_with_cache, run_jit_or_native, should_lookup_jit,
    BenchEvm, BenchError, BinLoader, JitHandler, NativeHandler, OpCtx,
};

// ── Frame-tracing handler ────────────────────────────────────────────────────

struct FrameTracer {
    pub frames: Vec<(B256, usize, bool)>,  // (bytecode_hash, bytecode_size, is_jit)
    pub jit_fns: std::sync::Arc<HashMap<B256, revmc_context::RawEvmCompilerFn>>,
    pub loop_iters: usize,
    // Step timings (nanoseconds)
    pub t_hash: u64,
    pub t_lookup: u64,
    pub t_frame_exec: u64,
    pub t_frame_return: u64,
}

impl Handler for FrameTracer {
    type Evm = BenchEvm;
    type Error = BenchError;
    type HaltReason = op_revm::OpHaltReason;

    fn run_exec_loop(
        &mut self,
        evm: &mut Self::Evm,
        first_frame: FrameInit,
    ) -> Result<FrameResult, Self::Error> {
        let res = evm.frame_init(first_frame)?;
        if let ItemOrResult::Result(r) = res { return Ok(r); }
        loop {
            self.loop_iters += 1;

            let t0 = Instant::now();
            let (hash, size) = {
                let frame = evm.0.frame_stack.get();
                let h = frame.interpreter.bytecode.get_or_calculate_hash();
                let sz = frame.interpreter.bytecode.original_byte_slice().len();
                (h, sz)
            };
            self.t_hash += t0.elapsed().as_nanos() as u64;

            let t1 = Instant::now();
            let frame = evm.0.frame_stack.get();
            let is_jit = should_lookup_jit(
                frame.data.is_create(),
                frame.interpreter.input.bytecode_address,
                frame.interpreter.bytecode.is_empty(),
            ) && self.jit_fns.contains_key(&hash);
            self.t_lookup += t1.elapsed().as_nanos() as u64;
            self.frames.push((hash, size, is_jit));

            let t2 = Instant::now();
            let call_or_result = run_jit_or_native(evm, &self.jit_fns)?;
            self.t_frame_exec += t2.elapsed().as_nanos() as u64;

            let result = match call_or_result {
                ItemOrResult::Item(init) => match evm.frame_init(init)? {
                    ItemOrResult::Item(_) => continue,
                    ItemOrResult::Result(r) => r,
                },
                ItemOrResult::Result(r) => r,
            };
            let t3 = Instant::now();
            if let Some(r) = evm.frame_return_result(result)? {
                self.t_frame_return += t3.elapsed().as_nanos() as u64;
                return Ok(r);
            }
            self.t_frame_return += t3.elapsed().as_nanos() as u64;
        }
    }
}

const TX_IDX: usize = 130;

// ── Opcode decoding ──────────────────────────────────────────────────────────

fn opcode_name(op: u8) -> &'static str {
    match op {
        0x00 => "STOP",       0x01 => "ADD",        0x02 => "MUL",
        0x03 => "SUB",        0x04 => "DIV",        0x05 => "SDIV",
        0x06 => "MOD",        0x07 => "SMOD",       0x08 => "ADDMOD",
        0x09 => "MULMOD",     0x0a => "EXP",        0x0b => "SIGNEXTEND",
        0x10 => "LT",         0x11 => "GT",         0x12 => "SLT",
        0x13 => "SGT",        0x14 => "EQ",         0x15 => "ISZERO",
        0x16 => "AND",        0x17 => "OR",         0x18 => "XOR",
        0x19 => "NOT",        0x1a => "BYTE",       0x1b => "SHL",
        0x1c => "SHR",        0x1d => "SAR",
        0x20 => "KECCAK256",
        0x30 => "ADDRESS",    0x31 => "BALANCE",    0x32 => "ORIGIN",
        0x33 => "CALLER",     0x34 => "CALLVALUE",  0x35 => "CALLDATALOAD",
        0x36 => "CALLDATASIZE",0x37 => "CALLDATACOPY",0x38 => "CODESIZE",
        0x39 => "CODECOPY",   0x3a => "GASPRICE",   0x3b => "EXTCODESIZE",
        0x3c => "EXTCODECOPY",0x3d => "RETURNDATASIZE",0x3e => "RETURNDATACOPY",
        0x3f => "EXTCODEHASH",0x40 => "BLOCKHASH",  0x41 => "COINBASE",
        0x42 => "TIMESTAMP",  0x43 => "NUMBER",     0x44 => "PREVRANDAO",
        0x45 => "GASLIMIT",   0x46 => "CHAINID",    0x47 => "SELFBALANCE",
        0x48 => "BASEFEE",
        0x50 => "POP",        0x51 => "MLOAD",      0x52 => "MSTORE",
        0x53 => "MSTORE8",    0x54 => "SLOAD",      0x55 => "SSTORE",
        0x56 => "JUMP",       0x57 => "JUMPI",      0x58 => "PC",
        0x59 => "MSIZE",      0x5a => "GAS",        0x5b => "JUMPDEST",
        0x5c => "TLOAD",      0x5d => "TSTORE",
        0x60..=0x7f => "PUSHn",
        0x80..=0x8f => "DUPn", 0x90..=0x9f => "SWAPn",
        0xa0 => "LOG0", 0xa1 => "LOG1", 0xa2 => "LOG2", 0xa3 => "LOG3", 0xa4 => "LOG4",
        0xf0 => "CREATE",     0xf1 => "CALL",       0xf2 => "CALLCODE",
        0xf3 => "RETURN",     0xf4 => "DELEGATECALL",0xf5 => "CREATE2",
        0xfa => "STATICCALL", 0xfd => "REVERT",     0xfe => "INVALID",
        0xff => "SELFDESTRUCT",
        _ => "UNKNOWN",
    }
}

fn decode_first_n_ops(bytes: &[u8], n: usize) -> Vec<(usize, u8, String)> {
    let mut ops = Vec::new();
    let mut i = 0;
    while i < bytes.len() && ops.len() < n {
        let op = bytes[i];
        let name = opcode_name(op);
        let detail = if op >= 0x60 && op <= 0x7f {
            let push_size = (op - 0x5f) as usize;
            let end = (i + 1 + push_size).min(bytes.len());
            let val: String = hex::encode(&bytes[i + 1..end]);
            format!("PUSH{} 0x{}", push_size, val)
        } else {
            name.to_string()
        };
        ops.push((i, op, detail));
        if op >= 0x60 && op <= 0x7f {
            i += (op - 0x5f) as usize;
        }
        i += 1;
    }
    ops
}

// Full opcode histogram (skip PUSH immediates)
fn opcode_histogram(bytes: &[u8]) -> Vec<(u32, u8, &'static str)> {
    let mut counts = [0u32; 256];
    let mut i = 0;
    while i < bytes.len() {
        let op = bytes[i];
        counts[op as usize] += 1;
        if op >= 0x60 && op <= 0x7f { i += (op - 0x5f) as usize; }
        i += 1;
    }
    let mut hist: Vec<(u32, u8, &'static str)> = (0u16..=255)
        .filter(|&op| counts[op as usize] > 0)
        .map(|op| (counts[op as usize], op as u8, opcode_name(op as u8)))
        .collect();
    hist.sort_by(|a, b| b.0.cmp(&a.0));
    hist
}

// ── Per-call timing (run tx many times, report precise timing) ──────────────

fn time_tx_ns(
    loader: &BinLoader,
    functions: Option<&std::sync::Arc<HashMap<B256, revmc_context::RawEvmCompilerFn>>>,
    rounds: usize,
) -> Vec<u64> {
    let chain_id = loader.raw_txs().first().and_then(|tx| tx.chain_id);
    let cfg = build_op_cfg(chain_id);
    let dummy_tx = OpTransaction::builder().build_fill();
    let tx_bin = &loader.raw_txs()[TX_IDX];
    let mut samples = Vec::with_capacity(rounds);

    for _ in 0..rounds {
        let mut evm: BenchEvm = {
            let ctx = OpCtx::<EmptyDB>::op()
                .with_cfg(cfg.clone())
                .with_db(loader.build_cache_db())
                .with_tx(dummy_tx.clone());
            op_revm::OpEvm::new(ctx, ())
        };
        evm.0.ctx.tx = build_op_tx(tx_bin);
        let t0 = Instant::now();
        if let Some(fns) = functions {
            let mut h = JitHandler { functions: fns.clone() };
            let _ = h.run(&mut evm);
        } else {
            let mut h = NativeHandler;
            let _ = h.run(&mut evm);
        }
        samples.push(t0.elapsed().as_nanos() as u64);
    }
    samples
}

fn time_tx_ns_empty_jit(loader: &BinLoader, rounds: usize) -> Vec<u64> {
    // JitHandler with EMPTY functions map — pays dispatch overhead but no JIT functions loaded
    let empty: std::sync::Arc<HashMap<B256, revmc_context::RawEvmCompilerFn>> =
        std::sync::Arc::new(HashMap::new());
    time_tx_ns(loader, Some(&empty), rounds)
}

// ── CLI ──────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "analyze_create_tx")]
struct Args {
    #[arg(long, default_value = "/home/ubuntu/sipeng/bench_data")]
    dir: String,
    #[arg(long, default_value_t = 38004930)]
    block: u64,
    #[arg(long, default_value = "./jit_cache")]
    cache_dir: String,
    #[arg(long, default_value_t = 20)]
    rounds: usize,
    #[arg(long, default_value_t = 5)]
    warmup: usize,
}

fn main() {
    let args = Args::parse();
    let loader = BinLoader::new(Path::new(&args.dir), args.block).expect("load block");

    let tx = &loader.raw_txs()[TX_IDX];
    let initcode = &tx.data;

    println!("=== CREATE tx[{TX_IDX}] deep-dive ===\n");
    println!("caller:    0x{}", hex::encode(&tx.caller));
    println!("gas_limit: {}", tx.gas_limit);
    println!("initcode:  {}B\n", initcode.len());

    // ── 1. Is the initcode JIT-compiled? ─────────────────────────────────────
    let init_hash = keccak256(initcode);
    println!("── 1. JIT compilation check ──────────────────────────────────");
    println!("initcode keccak256: {}", hex::encode(init_hash.as_slice()));

    let in_snapshot = loader.code_values().contains_key(&init_hash);
    println!("in snapshot codes:  {}", if in_snapshot { "YES → will be JIT-compiled" } else { "NO  → falls back to interpreter" });

    // Check cache directory
    let cache_file = Path::new(&args.cache_dir)
        .join(format!("{}__{}__{}.so", hex::encode(init_hash.as_slice()), "prague", 3));
    println!("cache file exists:  {}", if cache_file.exists() { "YES" } else { "NO" });
    println!();

    // ── 2. Opcode analysis ────────────────────────────────────────────────────
    println!("── 2. Initcode opcode analysis ───────────────────────────────");
    let hist = opcode_histogram(initcode);
    let total_ops: u32 = hist.iter().map(|(c, _, _)| c).sum();
    println!("total opcodes: {}", total_ops);
    println!("top-20 by frequency:");
    println!("  {:>6}  {:>4}  {:>10}  {}", "count", "op", "name", "fraction");
    for &(count, op, name) in hist.iter().take(20) {
        let frac = count as f64 / total_ops as f64;
        let bar = "#".repeat((frac * 60.0) as usize);
        println!("  {:>6}  0x{:02x}  {:>10}  {:.3}  {}", count, op, name, frac, bar);
    }
    println!();

    // ── 3. First 25 opcodes (constructor header) ──────────────────────────────
    println!("── 3. First 25 decoded opcodes (constructor header) ──────────");
    let ops = decode_first_n_ops(initcode, 25);
    for (offset, op, detail) in &ops {
        println!("  [{:05x}] 0x{:02x}  {}", offset, op, detail);
    }
    println!();

    // ── 4. Storage op analysis ────────────────────────────────────────────────
    println!("── 4. Storage op breakdown ───────────────────────────────────");
    let sload_count = hist.iter().find(|(_, op, _)| *op == 0x54).map(|(c,_,_)| *c).unwrap_or(0);
    let sstore_count = hist.iter().find(|(_, op, _)| *op == 0x55).map(|(c,_,_)| *c).unwrap_or(0);
    let tload_count = hist.iter().find(|(_, op, _)| *op == 0x5c).map(|(c,_,_)| *c).unwrap_or(0);
    let tstore_count = hist.iter().find(|(_, op, _)| *op == 0x5d).map(|(c,_,_)| *c).unwrap_or(0);
    println!("  SLOAD:  {} ({:.1}%)", sload_count, sload_count as f64 / total_ops as f64 * 100.0);
    println!("  SSTORE: {} ({:.1}%)", sstore_count, sstore_count as f64 / total_ops as f64 * 100.0);
    println!("  TLOAD:  {}", tload_count);
    println!("  TSTORE: {}", tstore_count);
    println!("  Total storage ops: {} in {} total = {:.1}%",
        sload_count + sstore_count + tload_count + tstore_count,
        total_ops,
        (sload_count + sstore_count + tload_count + tstore_count) as f64 / total_ops as f64 * 100.0);
    println!();

    // ── 5. Compile and benchmark ──────────────────────────────────────────────
    println!("── 5. Benchmark (isolated, {} warmup + {} rounds) ────────────",
        args.warmup, args.rounds);
    let all_codes: std::collections::HashMap<B256, _> = loader.code_values().iter()
        .map(|(h, bc)| (*h, bc.clone()))
        .collect();
    let compiled = compile_all_contracts_with_cache(
        &all_codes, OptimizationLevel::Aggressive, Path::new(&args.cache_dir),
    );
    eprintln!("  {} JIT functions loaded", compiled.functions.len());

    // Warmup
    time_tx_ns(&loader, None, args.warmup);
    time_tx_ns(&loader, Some(&compiled.functions), args.warmup);
    time_tx_ns_empty_jit(&loader, args.warmup);

    // Measure
    let mut native_ns   = time_tx_ns(&loader, None, args.rounds);
    let mut jit_ns      = time_tx_ns(&loader, Some(&compiled.functions), args.rounds);
    let mut empty_jit_ns = time_tx_ns_empty_jit(&loader, args.rounds);
    native_ns.sort_unstable();
    jit_ns.sort_unstable();
    empty_jit_ns.sort_unstable();

    let n_med  = native_ns[args.rounds / 2]    as f64 / 1000.0;
    let j_med  = jit_ns[args.rounds / 2]       as f64 / 1000.0;
    let ej_med = empty_jit_ns[args.rounds / 2] as f64 / 1000.0;
    let n_min  = native_ns[0]    as f64 / 1000.0;
    let j_min  = jit_ns[0]       as f64 / 1000.0;
    let ej_min = empty_jit_ns[0] as f64 / 1000.0;

    println!("  Native:         median={:.1}µs  min={:.1}µs", n_med, n_min);
    println!("  JIT (304 fns):  median={:.1}µs  min={:.1}µs  overhead={:.1}µs", j_med,  j_min,  j_med  - n_med);
    println!("  JIT (empty map):median={:.1}µs  min={:.1}µs  overhead={:.1}µs", ej_med, ej_min, ej_med - n_med);
    println!();
    println!("  If 'empty map' ≈ native: overhead comes from JIT-compiled .so memory pressure");
    println!("  If 'empty map' ≈ JIT:    overhead comes from dispatch logic itself");
    println!();

    // ── 6. Overhead attribution ───────────────────────────────────────────────
    println!("── 6. Overhead attribution ───────────────────────────────────");
    let overhead_us = j_med - n_med;
    let storage_total = sload_count + sstore_count;
    if in_snapshot && storage_total > 0 {
        let per_storage_ns = overhead_us * 1000.0 / storage_total as f64;
        println!("  JIT compiled initcode: YES");
        println!("  Storage ops: {} SLOAD + {} SSTORE = {} total", sload_count, sstore_count, storage_total);
        println!("  Overhead: {:.1}µs / {} storage ops = {:.0}ns per SLOAD/SSTORE FFI overhead",
            overhead_us, storage_total, per_storage_ns);
    } else {
        println!("  JIT compiled initcode: NO (interpreter fallback)");
        println!("  Overhead likely from CALL sub-frames hitting JIT during construction");
    }

    // ── 7. Frame trace ────────────────────────────────────────────────────────
    println!("\n── 7. Frame trace (which frames execute during construction) ─");
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
    trace_evm.0.ctx.tx = build_op_tx(tx);
    let mut tracer = FrameTracer {
        frames: Vec::new(),
        jit_fns: compiled.functions.clone(),
        loop_iters: 0,
        t_hash: 0, t_lookup: 0, t_frame_exec: 0, t_frame_return: 0,
    };
    let _ = tracer.run(&mut trace_evm);

    println!("  Total frames: {}", tracer.frames.len());
    let jit_frames = tracer.frames.iter().filter(|(_, _, j)| *j).count();
    let native_frames = tracer.frames.len() - jit_frames;
    println!("  JIT frames:   {} ({:.0}%)",
        jit_frames, jit_frames as f64 / tracer.frames.len() as f64 * 100.0);
    println!("  Native frames:{} ({:.0}%)",
        native_frames, native_frames as f64 / tracer.frames.len() as f64 * 100.0);

    // Unique bytecodes per frame type
    let mut jit_sizes: Vec<usize> = tracer.frames.iter().filter(|(_, _, j)| *j).map(|(_, s, _)| *s).collect();
    let mut nat_sizes: Vec<usize> = tracer.frames.iter().filter(|(_, _, j)| !*j).map(|(_, s, _)| *s).collect();
    jit_sizes.sort_unstable();
    nat_sizes.sort_unstable();

    println!();
    println!("  Frame breakdown:");
    println!("  {:>5}  {:>5}  {:>8}  {:<12}", "frame", "size", "jit?", "hash_prefix");
    for (i, (hash, size, is_jit)) in tracer.frames.iter().enumerate() {
        println!("  {:>5}  {:>5}B  {:>8}  {}",
            i, size,
            if *is_jit { "JIT" } else { "native" },
            &hex::encode(hash.as_slice())[..12]);
    }

    println!();
    println!("  loop_iters:      {}", tracer.loop_iters);
    println!("  t_hash:          {:.1}µs  ← get_or_calculate_hash()", tracer.t_hash as f64 / 1000.0);
    println!("  t_lookup:        {:.1}µs  ← HashMap::get()", tracer.t_lookup as f64 / 1000.0);
    println!("  t_frame_exec:    {:.1}µs  ← run_jit_or_native()", tracer.t_frame_exec as f64 / 1000.0);
    println!("  t_frame_return:  {:.1}µs  ← evm.frame_return_result()", tracer.t_frame_return as f64 / 1000.0);
    println!("  sum:             {:.1}µs", (tracer.t_hash + tracer.t_lookup + tracer.t_frame_exec + tracer.t_frame_return) as f64 / 1000.0);
    println!();
    println!("  JIT frame sizes:    {:?}", &jit_sizes[..jit_sizes.len().min(10)]);
    println!("  Native frame sizes: {:?}", &nat_sizes[..nat_sizes.len().min(5)]);

    // ── 8. Keccak baseline ─────────────────────────────────────────────────
    println!("\n── 8. keccak256 baseline for {:.1}KB ─────────────────────────", initcode.len() as f64 / 1024.0);
    // Warm the data into cache first
    let _ = keccak256(initcode.as_slice());
    let mut keccak_times_ns: Vec<u64> = (0..100).map(|_| {
        let t = Instant::now();
        let _ = std::hint::black_box(keccak256(std::hint::black_box(initcode.as_slice())));
        t.elapsed().as_nanos() as u64
    }).collect();
    keccak_times_ns.sort_unstable();
    let kec_min = keccak_times_ns[0] as f64 / 1000.0;
    let kec_med = keccak_times_ns[50] as f64 / 1000.0;
    println!("  keccak256({:.1}KB): min={:.2}µs  median={:.2}µs", initcode.len() as f64 / 1024.0, kec_min, kec_med);
    println!("  → If hash() takes 65µs but keccak alone takes {:.2}µs,", kec_med);
    println!("    the extra ~{:.1}µs must come from bytecode analysis (LegacyRaw→LegacyAnalyzed).", 65.0 - kec_med);
    println!();
    // Also time hash_slow on an actually analyzed bytecode
    {
        use revm::bytecode::{Bytecode, LegacyRawBytecode};
        // Analyze it via LegacyRawBytecode
        let raw2 = LegacyRawBytecode(revm::primitives::Bytes::from(initcode.clone()));
        let analyzed = Bytecode::LegacyAnalyzed(raw2.into_analyzed());
        let mut keccak_analyzed_ns: Vec<u64> = (0..100).map(|_| {
            let t = Instant::now();
            let _ = std::hint::black_box(analyzed.hash_slow());
            t.elapsed().as_nanos() as u64
        }).collect();
        keccak_analyzed_ns.sort_unstable();
        let ka_min = keccak_analyzed_ns[0] as f64 / 1000.0;
        let ka_med = keccak_analyzed_ns[50] as f64 / 1000.0;
        println!("  hash_slow() on LegacyAnalyzed: min={:.2}µs  median={:.2}µs", ka_min, ka_med);

        // Time the analysis itself
        let mut analyze_ns: Vec<u64> = (0..20).map(|_| {
            let fresh = LegacyRawBytecode(revm::primitives::Bytes::from(initcode.clone()));
            let t = Instant::now();
            let _ = std::hint::black_box(fresh.into_analyzed());
            t.elapsed().as_nanos() as u64
        }).collect();
        analyze_ns.sort_unstable();
        let an_min = analyze_ns[0] as f64 / 1000.0;
        let an_med = analyze_ns[10] as f64 / 1000.0;
        println!("  to_analysed() (JUMPDEST scan): min={:.2}µs  median={:.2}µs", an_min, an_med);
    }
}
