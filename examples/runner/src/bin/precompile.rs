//! Batch pre-compiler: scans all blocks from bench_data, deduplicates contracts,
//! and compiles them to persistent AOT cache with resume support.
//!
//! Usage:
//!   cargo run -p revmc-examples-runner --bin precompile --release -- \
//!     --cache-dir /tmp/jit_cache
//!   cargo run -p revmc-examples-runner --bin precompile --release -- \
//!     --cache-dir /tmp/jit_cache --start 38004930 --count 100 --threads 16

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use clap::Parser;
use revm::{
    bytecode::Bytecode,
    primitives::{Address, B256, HashMap as RevmHashMap, U256},
    state::AccountInfo,
};
use revmc::{EvmCompiler, EvmLlvmBackend, OptimizationLevel};
use serde::Deserialize;

use op_revm::OpSpecId;
use revm::primitives::hardfork::SpecId;

// ── Spec Constants ──────────────────────────────────────────────────────────

const OP_SPEC: OpSpecId = OpSpecId::ISTHMUS;
const ETH_SPEC: SpecId = OP_SPEC.into_eth_spec();

// ── Data Types (only what's needed for scanning) ────────────────────────────

#[derive(Deserialize)]
struct AccountSnapshot {
    #[allow(dead_code)]
    info: Option<AccountInfo>,
    #[allow(dead_code)]
    storage: RevmHashMap<U256, U256>,
}

#[derive(Deserialize)]
struct CacheSnapshot {
    #[allow(dead_code)]
    block_number: u64,
    #[allow(dead_code)]
    has_state_clear: bool,
    #[allow(dead_code)]
    accounts: RevmHashMap<Address, AccountSnapshot>,
    codes: RevmHashMap<B256, Bytecode>,
}

// ── AOT Cache (inlined from bin_bench) ──────────────────────────────────────

struct CacheArtifacts {
    key: String,
    symbol: String,
    object: PathBuf,
    library: PathBuf,
}

fn cache_artifacts(cache_dir: &Path, hash: &B256, opt: OptimizationLevel) -> CacheArtifacts {
    let hash_hex = hex::encode(hash);
    let spec_tag = format!("{ETH_SPEC:?}").to_lowercase();
    let opt_tag = match opt {
        OptimizationLevel::None => "o0",
        OptimizationLevel::Less => "o1",
        OptimizationLevel::Default => "o2",
        OptimizationLevel::Aggressive => "o3",
    };
    let key = format!("{hash_hex}__{spec_tag}__{opt_tag}");
    let stem = cache_dir.join(&key);
    CacheArtifacts {
        key,
        symbol: format!("c_{hash_hex}"),
        object: stem.with_extension("o"),
        library: stem.with_extension(std::env::consts::DLL_EXTENSION),
    }
}

fn compile_contract_to_cache(
    bytecode: &Bytecode,
    opt: OptimizationLevel,
    artifacts: &CacheArtifacts,
) -> Result<(), String> {
    if let Some(parent) = artifacts.object.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let context = Box::leak(Box::new(revmc::llvm::inkwell::context::Context::create()));
    let backend =
        EvmLlvmBackend::new(context, true, opt).map_err(|e| format!("AOT backend: {e}"))?;
    let mut compiler = EvmCompiler::new(backend);
    compiler
        .translate(
            &artifacts.symbol,
            bytecode.original_byte_slice(),
            ETH_SPEC,
        )
        .map_err(|e| format!("translate {}: {e}", artifacts.key))?;
    compiler
        .write_object_to_file(&artifacts.object)
        .map_err(|e| format!("write {}: {e}", artifacts.object.display()))?;
    revmc::Linker::new()
        .link(&artifacts.library, [&artifacts.object])
        .map_err(|e| format!("link {}: {e}", artifacts.library.display()))?;
    Ok(())
}

// ── CLI ─────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "precompile",
    about = "Batch-compile all EVM contracts from bench_data to persistent AOT cache"
)]
struct Args {
    /// Path to bench_data directory
    #[arg(long, default_value = "/home/ubuntu/sipeng/bench_data")]
    dir: String,

    /// AOT cache output directory (required)
    #[arg(long)]
    cache_dir: String,

    /// First block number
    #[arg(long, default_value_t = 38004930)]
    start: u64,

    /// Number of blocks to scan
    #[arg(long, default_value_t = 10)]
    count: u64,

    /// Step between sampled blocks (e.g. 1000 = every 1000th block)
    #[arg(long, default_value_t = 1000)]
    step: u64,

    /// Number of compilation threads (0 = auto-detect)
    #[arg(long, default_value_t = 0)]
    threads: usize,

    /// Optimization level: 0=None, 1=Less, 2=Default, 3=Aggressive
    #[arg(long, default_value_t = 3)]
    opt_level: u8,
}

// ── Main ────────────────────────────────────────────────────────────────────

fn main() {
    let args = Args::parse();
    let bench_dir = Path::new(&args.dir);
    let cache_dir = Path::new(&args.cache_dir);
    let opt = match args.opt_level {
        0 => OptimizationLevel::None,
        1 => OptimizationLevel::Less,
        2 => OptimizationLevel::Default,
        _ => OptimizationLevel::Aggressive,
    };
    let n_threads = if args.threads == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get().min(16))
            .unwrap_or(4)
    } else {
        args.threads
    };

    // Build the list of block numbers to scan
    let block_numbers: Vec<u64> = (0..args.count)
        .map(|i| args.start + i * args.step)
        .collect();

    println!(
        "=== Batch Pre-compile: {} blocks (start={}, step={}) ===",
        block_numbers.len(),
        args.start,
        args.step,
    );
    println!(
        "  Blocks: {:?}{}",
        &block_numbers[..block_numbers.len().min(5)],
        if block_numbers.len() > 5 { " ..." } else { "" }
    );
    println!("  Cache dir: {}", cache_dir.display());
    println!("  Threads: {n_threads}");
    println!("  Opt level: {:?}\n", opt);

    // ── Phase 1: Scan selected blocks, collect unique bytecodes ─────────
    println!("=== Phase 1: Scanning blocks ===");
    let scan_start = Instant::now();
    let mut all_codes: HashMap<B256, Bytecode> = HashMap::new();
    let mut scanned = 0u64;
    let mut scan_errors = 0u64;

    for &bn in &block_numbers {
        let path = bench_dir.join(format!("states/{bn}.bin"));
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(_) => {
                scan_errors += 1;
                continue;
            }
        };
        let snapshot: CacheSnapshot = match bincode::deserialize(&data) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  WARN: skip block {bn}: {e}");
                scan_errors += 1;
                continue;
            }
        };
        for (hash, bytecode) in snapshot.codes {
            all_codes.entry(hash).or_insert(bytecode);
        }
        scanned += 1;
        if scanned % 1000 == 0 {
            eprintln!(
                "  {scanned} blocks scanned, {} unique codes so far",
                all_codes.len()
            );
        }
    }
    let scan_dur = scan_start.elapsed();
    println!(
        "  Scanned {scanned} blocks ({scan_errors} skipped) in {:.1}s",
        scan_dur.as_secs_f64()
    );
    println!("  Total unique bytecodes: {}\n", all_codes.len());

    // ── Phase 2: Filter cached & empty, prepare compilation list ────────
    std::fs::create_dir_all(cache_dir).ok();
    let mut to_compile: Vec<(B256, Bytecode)> = Vec::new();
    let mut cached = 0usize;
    let mut empty = 0usize;

    for (hash, bytecode) in &all_codes {
        if bytecode.is_empty() {
            empty += 1;
            continue;
        }
        let artifacts = cache_artifacts(cache_dir, hash, opt);
        if artifacts.object.exists() && artifacts.library.exists() {
            cached += 1;
        } else {
            to_compile.push((*hash, bytecode.clone()));
        }
    }
    // Sort by size descending for better load balancing across threads
    to_compile.sort_by_key(|(_, bc)| std::cmp::Reverse(bc.original_byte_slice().len()));

    println!("=== Phase 2: Compiling ===");
    println!("  Already cached: {cached}");
    println!("  Empty (skip): {empty}");
    println!("  To compile: {}\n", to_compile.len());

    if to_compile.is_empty() {
        println!("Nothing to compile. All contracts are cached.");
        return;
    }

    // ── Phase 3: Parallel compile with per-contract persistence ─────────
    let mut assignments: Vec<Vec<(B256, Bytecode)>> = (0..n_threads).map(|_| Vec::new()).collect();
    for (i, item) in to_compile.into_iter().enumerate() {
        assignments[i % n_threads].push(item);
    }

    let compile_start = Instant::now();
    let total: usize = assignments.iter().map(|a| a.len()).sum();
    let compiled_count = AtomicUsize::new(0);
    let failed_count = AtomicUsize::new(0);

    std::thread::scope(|s| {
        for (tid, chunk) in assignments.into_iter().enumerate() {
            let compiled_ref = &compiled_count;
            let failed_ref = &failed_count;
            s.spawn(move || {
                for (hash, bytecode) in &chunk {
                    let artifacts = cache_artifacts(cache_dir, hash, opt);
                    match compile_contract_to_cache(bytecode, opt, &artifacts) {
                        Ok(()) => {
                            let done =
                                compiled_ref.fetch_add(1, Ordering::Relaxed) + 1;
                            if done % 50 == 0 || done == total {
                                let elapsed = compile_start.elapsed().as_secs_f64();
                                let rate = done as f64 / elapsed;
                                let remaining = (total - done) as f64 / rate;
                                let eta_h = remaining / 3600.0;
                                let eta_m = (remaining % 3600.0) / 60.0;
                                eprintln!(
                                    "  [{done}/{total}] {:.1}s elapsed, ETA ~{:.0}h{:.0}m ({:.2} contracts/s)",
                                    elapsed, eta_h, eta_m, rate
                                );
                            }
                        }
                        Err(e) => {
                            failed_ref.fetch_add(1, Ordering::Relaxed);
                            eprintln!(
                                "  [T{tid}] FAIL {}: {e}",
                                &hex::encode(hash)[..16]
                            );
                        }
                    }
                }
            });
        }
    });

    let compile_dur = compile_start.elapsed();
    let compiled = compiled_count.load(Ordering::Relaxed);
    let failed = failed_count.load(Ordering::Relaxed);

    println!("\n=== Summary ===");
    println!("  Compiled: {compiled}");
    println!("  Failed: {failed}");
    println!("  Previously cached: {cached}");
    println!(
        "  Compile time: {:.1}s ({:.2} contracts/s)",
        compile_dur.as_secs_f64(),
        compiled as f64 / compile_dur.as_secs_f64().max(0.001)
    );
    println!("  Total cache: {} contracts", cached + compiled);
}
