//! PGO single-contract benchmark: profile with Inspector, then compare JIT vs JIT+PGO.
//!
//! Tests: Burntpix, Snailtracer, Uniswap V3, Curve
//!
//! Usage:
//!   cargo run -p revmc-examples-runner --bin pgo_single_bench --release

#[path = "../bench_common.rs"]
mod bench_common;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use revm::context::{BlockEnv, CfgEnv, TxEnv};
use revm::context_interface::ContextSetters;
use revm::database::{CacheDB, EmptyDB};
use revm::handler::{Handler, MainBuilder};
use revm::inspector::InspectEvm;
use revm::interpreter::interpreter::EthInterpreter;
use revm::interpreter::interpreter_types::Jumps;
use revm::interpreter::Interpreter;
use revm::primitives::hardfork::SpecId;
use revm::primitives::{Address, B256};
use revm::state::AccountInfo;
use revm::{ExecuteEvm, Inspector};
use revmc::profile::BranchProfile;
use revmc::{EvmCompiler, EvmLlvmBackend, OptimizationLevel};
use revmc_builtins as _;
use revmc_context::RawEvmCompilerFn;

type BenchEvm<'a> = revm::MainnetEvm<revm::handler::MainnetContext<&'a mut CacheDB<EmptyDB>>>;

// JUMPI opcode
const JUMPI: u8 = 0x57;
const OPT: OptimizationLevel = OptimizationLevel::Aggressive;

// ── BranchCollector Inspector ────────────────────────────────────────────────

struct BranchCollector {
    profiles: HashMap<B256, BranchProfile>,
    current_hash: B256,
}

impl BranchCollector {
    fn new() -> Self {
        Self {
            profiles: HashMap::new(),
            current_hash: B256::ZERO,
        }
    }
}

impl<CTX> Inspector<CTX, EthInterpreter> for BranchCollector {
    fn initialize_interp(&mut self, interp: &mut Interpreter<EthInterpreter>, _ctx: &mut CTX) {
        self.current_hash = interp.bytecode.get_or_calculate_hash();
    }

    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _ctx: &mut CTX) {
        if interp.bytecode.opcode() != JUMPI {
            return;
        }
        let pc = interp.bytecode.pc() as u32;
        let data = interp.stack.data();
        if data.len() < 2 {
            return;
        }
        // JUMPI: stack[top] = destination, stack[top-1] = condition
        let condition = data[data.len() - 2];
        let taken = !condition.is_zero();
        self.profiles
            .entry(self.current_hash)
            .or_default()
            .record(pc, taken);
    }
}

// ── JIT compilation with optional PGO ────────────────────────────────────────

struct CompiledFunctions {
    functions: Arc<HashMap<B256, RawEvmCompilerFn>>,
    // Keep compiler alive so JIT code stays valid
    _compiler: &'static mut EvmCompiler<EvmLlvmBackend<'static>>,
    _context: &'static revmc::llvm::inkwell::context::Context,
}

fn compile_contracts_pgo(
    accounts: &[(Address, AccountInfo)],
    profile: Option<&HashMap<B256, BranchProfile>>,
) -> Result<CompiledFunctions, String> {
    let context = Box::leak(Box::new(revmc::llvm::inkwell::context::Context::create()));
    let backend =
        EvmLlvmBackend::new(context, false, OPT).map_err(|e| format!("backend: {e}"))?;
    let compiler: &'static mut EvmCompiler<EvmLlvmBackend<'static>> =
        Box::leak(Box::new(EvmCompiler::new(backend)));

    let mut seen = std::collections::HashSet::new();
    let mut pending = Vec::new();

    for (_addr, info) in accounts {
        let Some(code) = info.code.as_ref() else {
            continue;
        };
        if code.is_empty() {
            continue;
        }
        let hash = info.code_hash;
        if !seen.insert(hash) {
            continue;
        }

        // Set branch profile if available
        if let Some(profiles) = profile {
            if let Some(bp) = profiles.get(&hash) {
                compiler.set_branch_profile(bp.clone());
            } else {
                compiler.clear_branch_profile();
            }
        }

        let name = format!("contract_{}", hex::encode(hash.as_slice()));
        let func_id = compiler
            .translate(&name, code.original_byte_slice(), SpecId::CANCUN)
            .map_err(|e| format!("translate: {e}"))?;
        pending.push((hash, func_id));
    }

    let mut functions = HashMap::with_capacity(pending.len());
    for (hash, func_id) in pending {
        let fn_ptr = unsafe { compiler.jit_function(func_id).map_err(|e| format!("jit: {e}"))? };
        functions.insert(hash, fn_ptr.into_inner());
    }

    Ok(CompiledFunctions {
        functions: Arc::new(functions),
        _compiler: compiler,
        _context: context,
    })
}

// ── Fixture: load + Inspector profiling + execution ──────────────────────────

struct TestCase {
    name: &'static str,
    block: BlockEnv,
    cfg: CfgEnv,
    tx: TxEnv,
    db: Arc<CacheDB<EmptyDB>>,
    accounts: Vec<(Address, AccountInfo)>,
}

impl TestCase {
    fn from_json(name: &'static str, fixture_path: &str) -> Result<Self, String> {
        let fixture = bench_common::Fixture::load(fixture_path)?;
        let db = fixture.prebuilt_db().clone();
        let accounts: Vec<(Address, AccountInfo)> = db
            .cache
            .accounts
            .iter()
            .map(|(addr, cached)| (*addr, cached.info.clone()))
            .collect();

        Ok(Self {
            name,
            block: fixture.block().clone(),
            cfg: fixture.cfg().clone(),
            tx: fixture.tx().clone(),
            db,
            accounts,
        })
    }

    fn from_hex(name: &'static str, hex_path: &str, calldata: &[u8]) -> Result<Self, String> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join(hex_path);
        let hex_str =
            std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let trimmed = hex_str.trim().strip_prefix("0x").unwrap_or(hex_str.trim());
        let bytecode = hex::decode(trimmed).map_err(|e| format!("hex decode: {e}"))?;

        let fixture = bench_common::Fixture::from_bytecode(&bytecode, calldata)?;
        let db = fixture.prebuilt_db().clone();
        let accounts: Vec<(Address, AccountInfo)> = db
            .cache
            .accounts
            .iter()
            .map(|(addr, cached)| (*addr, cached.info.clone()))
            .collect();

        Ok(Self {
            name,
            block: fixture.block().clone(),
            cfg: fixture.cfg().clone(),
            tx: fixture.tx().clone(),
            db,
            accounts,
        })
    }

    fn make_evm(&self) -> BenchEvm<'static> {
        let db_ref = unsafe { &mut *(Arc::as_ptr(&self.db) as *mut CacheDB<EmptyDB>) };
        let ctx = revm::context::Context::new(db_ref, SpecId::CANCUN);
        let mut evm = ctx.build_mainnet();
        evm.ctx.block = self.block.clone();
        evm.ctx.cfg = self.cfg.clone();
        evm
    }

    /// Run with Inspector to collect branch profiles.
    fn collect_profile(&self) -> HashMap<B256, BranchProfile> {
        let db_ref = unsafe { &mut *(Arc::as_ptr(&self.db) as *mut CacheDB<EmptyDB>) };
        let ctx: revm::handler::MainnetContext<_> =
            revm::context::Context::new(db_ref, SpecId::CANCUN);
        let mut evm = ctx.build_mainnet_with_inspector(BranchCollector::new());
        evm.ctx.block = self.block.clone();
        evm.ctx.cfg = self.cfg.clone();

        let _ = evm.inspect_one_tx(self.tx.clone());
        evm.inspector.profiles
    }

    /// Run plain interpreter.
    fn run_plain(&self) -> Duration {
        let mut evm = self.make_evm();
        let t = Instant::now();
        let _ = evm.transact(self.tx.clone());
        t.elapsed()
    }

    /// Run JIT with given compiled functions.
    fn run_jit(&self, functions: &Arc<HashMap<B256, RawEvmCompilerFn>>) -> Duration {
        let mut evm = self.make_evm();
        evm.ctx.set_tx(self.tx.clone());
        let mut handler = bench_common::JitHandler {
            functions: functions.clone(),
        };
        let t = Instant::now();
        let _ = handler.run(&mut evm);
        t.elapsed()
    }
}

// ── Measurement ──────────────────────────────────────────────────────────────

fn median(times: &mut [Duration]) -> Duration {
    times.sort();
    let n = times.len();
    if n % 2 == 0 {
        (times[n / 2 - 1] + times[n / 2]) / 2
    } else {
        times[n / 2]
    }
}

const WARMUP: usize = 3;
const ITERS: usize = 10;

fn bench_case(case: &TestCase) {
    let sep = "=".repeat(60);
    println!("\n{sep}");
    println!("  {}", case.name);
    println!("{sep}");

    // Phase 1: Collect branch profile
    print!("  Profiling (Inspector)... ");
    let t = Instant::now();
    let profiles = case.collect_profile();
    let profile_dur = t.elapsed();

    let total_positions: usize = profiles.values().map(|p| p.branches.len()).sum();
    let total_samples: u64 = profiles
        .values()
        .flat_map(|p| p.branches.values())
        .map(|(t, nt)| t + nt)
        .sum();
    let cold_count: usize = profiles
        .values()
        .flat_map(|p| p.branches.iter())
        .filter(|(_, &(t, nt))| {
            let total = t + nt;
            total > 0 && (t * 5 < total || nt * 5 < total)
        })
        .count();

    println!("{:.3}ms", profile_dur.as_secs_f64() * 1000.0);
    println!(
        "  Profile: {} contracts, {} JUMPI positions, {} samples",
        profiles.len(),
        total_positions,
        total_samples
    );
    if total_positions > 0 {
        println!(
            "  Cold branches: {}/{} ({:.1}%)",
            cold_count,
            total_positions,
            cold_count as f64 / total_positions as f64 * 100.0
        );
    }

    // Phase 2: Compile (no PGO)
    print!("  Compiling (no PGO)... ");
    let t = Instant::now();
    let compiled_plain =
        compile_contracts_pgo(&case.accounts, None).expect("compile failed");
    println!("{:.3}ms", t.elapsed().as_secs_f64() * 1000.0);

    // Phase 3: Compile (with PGO)
    print!("  Compiling (PGO)... ");
    let t = Instant::now();
    let compiled_pgo =
        compile_contracts_pgo(&case.accounts, Some(&profiles)).expect("compile PGO failed");
    println!("{:.3}ms", t.elapsed().as_secs_f64() * 1000.0);

    // Phase 4: Benchmark
    println!("  Running {} warmup + {} measured iterations...", WARMUP, ITERS);

    // Warmup
    for _ in 0..WARMUP {
        case.run_plain();
        case.run_jit(&compiled_plain.functions);
        case.run_jit(&compiled_pgo.functions);
    }

    // Measure
    let mut plain_times = Vec::with_capacity(ITERS);
    let mut jit_times = Vec::with_capacity(ITERS);
    let mut pgo_times = Vec::with_capacity(ITERS);

    for _ in 0..ITERS {
        plain_times.push(case.run_plain());
        jit_times.push(case.run_jit(&compiled_plain.functions));
        pgo_times.push(case.run_jit(&compiled_pgo.functions));
    }

    let med_plain = median(&mut plain_times);
    let med_jit = median(&mut jit_times);
    let med_pgo = median(&mut pgo_times);

    let plain_ms = med_plain.as_secs_f64() * 1000.0;
    let jit_ms = med_jit.as_secs_f64() * 1000.0;
    let pgo_ms = med_pgo.as_secs_f64() * 1000.0;

    println!();
    println!("  {:<20} {:>10} {:>10} {:>10}", "Mode", "Median(ms)", "vs Native", "vs JIT");
    println!("  {:-<20} {:-<10} {:-<10} {:-<10}", "", "", "", "");
    println!(
        "  {:<20} {:>10.3} {:>10} {:>10}",
        "Native (interp)", plain_ms, "1.00x", "-"
    );
    println!(
        "  {:<20} {:>10.3} {:>9.2}x {:>10}",
        "JIT (no PGO)",
        jit_ms,
        plain_ms / jit_ms,
        "1.00x"
    );
    println!(
        "  {:<20} {:>10.3} {:>9.2}x {:>9.2}x",
        "JIT + PGO",
        pgo_ms,
        plain_ms / pgo_ms,
        jit_ms / pgo_ms
    );

    let delta_pct = (jit_ms - pgo_ms) / jit_ms * 100.0;
    if delta_pct.abs() > 0.5 {
        println!(
            "\n  PGO effect: {}{:.1}% vs JIT",
            if delta_pct > 0.0 { "+" } else { "" },
            delta_pct
        );
    } else {
        println!("\n  PGO effect: negligible ({:.1}%)", delta_pct);
    }
}

// ── Main ─────────────────────────────────────────────────────────────────────

fn main() {
    println!("=== PGO Single-Contract Benchmark ===");
    println!("Warmup: {WARMUP}, Measured: {ITERS}\n");

    let cases: Vec<TestCase> = vec![
        TestCase::from_json("Burntpix", "data/burntpix-benchmark.json")
            .expect("failed to load Burntpix"),
        TestCase::from_hex(
            "Snailtracer",
            "data/snailtracer.rt.hex",
            &[0x30, 0x62, 0x7b, 0x7c], // Benchmark() selector
        )
        .expect("failed to load Snailtracer"),
        TestCase::from_json("Uniswap V3", "data/uniswap-t100-c20.json")
            .expect("failed to load Uniswap V3"),
        TestCase::from_json("Curve StableSwap", "data/curve-stableswap-2pool.json")
            .expect("failed to load Curve"),
    ];

    for case in &cases {
        bench_case(case);
    }

    println!("\n=== Done ===");
}
