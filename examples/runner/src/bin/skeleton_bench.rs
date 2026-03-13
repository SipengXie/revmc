//! Skeleton-aware compilation + execution benchmark.
//!
//! Three execution modes compared:
//! 1. Native interpreter (baseline)
//! 2. Per-hash JIT (all contracts compiled individually)
//! 3. Skeleton JIT (skeleton groups share one compiled function + data tables)
//!
//! Two-tier AOT cache (Registry + .so):
//! - Per-hash contracts: existing AOT cache (`{code_hash}__{spec}__{opt}.so`)
//! - Skeleton groups: `skel_{skeleton_hash}__{opt}.so` + `skeleton_registry.bin`
//! - After first run, all subsequent runs load from cache.
//!
//! Usage:
//!   cargo run -p revmc-examples-runner --bin skeleton_bench --release [bench_dir] [block]

#[allow(dead_code)]
#[path = "../bin_common.rs"]
mod bin_common;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use op_revm::transaction::OpTransaction;
use op_revm::DefaultOp;
use revm::bytecode::Bytecode;
use revm::database::EmptyDB;
use revm::handler::{EvmTr, FrameResult, Handler, ItemOrResult};
use revm::primitives::B256;
use revmc::skeleton::{analyze_skeleton_group, build_data_table, PushClassification, SkeletonVariance};
use revmc::{EvmCompiler, EvmLlvmBackend, OptimizationLevel};
use revmc_builtins as _;
use revmc_context::{EvmCompilerFn, RawEvmCompilerFn};
use serde::{Deserialize, Serialize};

use bin_common::{
    build_op_cfg, build_op_tx, compile_all_contracts_with_cache, extract_gas, BenchEvm,
    BenchError, BinLoader, JitHandler, NativeHandler, OpCtx,
};

const OPT: OptimizationLevel = OptimizationLevel::Aggressive;

fn extract_opcode_skeleton(bytes: &[u8]) -> Vec<u8> {
    let mut skeleton = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let op = bytes[i];
        skeleton.push(op);
        i += 1;
        if op >= 0x60 && op <= 0x7f {
            i += (op - 0x5f) as usize;
        }
    }
    skeleton
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn opt_tag(opt: OptimizationLevel) -> &'static str {
    match opt {
        OptimizationLevel::None => "o0",
        OptimizationLevel::Less => "o1",
        OptimizationLevel::Default => "o2",
        OptimizationLevel::Aggressive => "o3",
    }
}

// ── Skeleton Registry ────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
struct SkeletonRegistry {
    groups: Vec<RegistryGroup>,
}

#[derive(Serialize, Deserialize)]
struct RegistryGroup {
    skeleton_hash: u64,
    member_hashes: Vec<[u8; 32]>,
    /// Variance per PUSH: 0 = Invariant, N > 0 = Variant(table_index = N - 1).
    push_variance: Vec<u32>,
    num_variant: u32,
}

impl RegistryGroup {
    fn from_analysis(skeleton_hash: u64, member_hashes: &[B256], variance: &SkeletonVariance) -> Self {
        let push_variance = variance
            .pushes
            .iter()
            .map(|p| match p {
                PushClassification::Invariant => 0,
                PushClassification::Variant { table_index } => table_index + 1,
            })
            .collect();
        Self {
            skeleton_hash,
            member_hashes: member_hashes.iter().map(|h| h.0).collect(),
            push_variance,
            num_variant: variance.num_variant,
        }
    }

}

fn registry_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join("skeleton_registry.bin")
}

fn save_registry(cache_dir: &Path, registry: &SkeletonRegistry) {
    std::fs::create_dir_all(cache_dir).ok();
    let data = bincode::serialize(registry).expect("serialize registry");
    std::fs::write(registry_path(cache_dir), data).expect("write registry");
}

// ── Skeleton AOT Cache ───────────────────────────────────────────────────────

struct SkeletonCacheArtifacts {
    symbol: String,
    object: PathBuf,
    library: PathBuf,
}

fn skeleton_cache_artifacts(cache_dir: &Path, skeleton_hash: u64) -> SkeletonCacheArtifacts {
    let tag = opt_tag(OPT);
    let spec_tag = format!("{:?}", bin_common::ETH_SPEC).to_lowercase();
    let key = format!("skel_{skeleton_hash:016x}__{spec_tag}__{tag}");
    let stem = cache_dir.join(&key);
    SkeletonCacheArtifacts {
        symbol: format!("skel_{skeleton_hash:016x}"),
        object: stem.with_extension("o"),
        library: stem.with_extension(std::env::consts::DLL_EXTENSION),
    }
}

fn compile_skeleton_to_cache(
    bytecode: &[u8],
    variance: &SkeletonVariance,
    artifacts: &SkeletonCacheArtifacts,
) -> Result<(), String> {
    if let Some(parent) = artifacts.object.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let context = Box::leak(Box::new(revmc::llvm::inkwell::context::Context::create()));
    let backend = EvmLlvmBackend::new(context, true, OPT)
        .map_err(|e| format!("AOT backend: {e}"))?;
    let mut compiler = EvmCompiler::new(backend);
    compiler
        .translate_skeleton(&artifacts.symbol, bytecode, bin_common::ETH_SPEC, variance)
        .map_err(|e| format!("translate_skeleton: {e}"))?;
    compiler
        .write_object_to_file(&artifacts.object)
        .map_err(|e| format!("write: {e}"))?;
    revmc::Linker::new()
        .link(&artifacts.library, [&artifacts.object])
        .map_err(|e| format!("link: {e}"))?;
    Ok(())
}

fn load_skeleton_cached(
    artifacts: &SkeletonCacheArtifacts,
) -> Result<(RawEvmCompilerFn, libloading::Library), String> {
    let library = unsafe { libloading::Library::new(&artifacts.library) }
        .map_err(|e| format!("dlopen: {e}"))?;
    let symbol = unsafe { library.get::<RawEvmCompilerFn>(artifacts.symbol.as_bytes()) }
        .map_err(|e| format!("dlsym: {e}"))?;
    let function = *symbol;
    drop(symbol);
    Ok((function, library))
}

// ── Skeleton Handler ─────────────────────────────────────────────────────────

struct SkeletonHandler {
    per_hash_fns: Arc<HashMap<B256, RawEvmCompilerFn>>,
    skeleton_dispatch: Arc<HashMap<B256, (EvmCompilerFn, Vec<u8>)>>,
}

impl Handler for SkeletonHandler {
    type Evm = BenchEvm;
    type Error = BenchError;
    type HaltReason = op_revm::OpHaltReason;

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
            let call_or_result = self.run_skeleton_or_jit(evm)?;
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
}

impl SkeletonHandler {
    fn run_skeleton_or_jit(
        &self,
        evm: &mut BenchEvm,
    ) -> Result<
        ItemOrResult<revm::interpreter::interpreter_action::FrameInit, FrameResult>,
        BenchError,
    > {
        let (ctx, frame_stack) = (&mut evm.0.ctx, &mut evm.0.frame_stack);
        let frame = frame_stack.get();

        if !bin_common::should_lookup_jit(
            frame.data.is_create(),
            frame.interpreter.input.bytecode_address,
            frame.interpreter.bytecode.is_empty(),
        ) {
            drop((ctx, frame_stack));
            return Ok(evm.frame_run()?);
        }

        let bytecode_hash = frame.interpreter.bytecode.get_or_calculate_hash();

        let action = if let Some((skel_fn, data_table)) = self.skeleton_dispatch.get(&bytecode_hash) {
            unsafe {
                skel_fn.call_with_interpreter_data(
                    &mut frame.interpreter,
                    ctx,
                    data_table.as_ptr(),
                )
            }
        } else if let Some(&raw_fn) = self.per_hash_fns.get(&bytecode_hash) {
            let f = EvmCompilerFn::new(raw_fn);
            unsafe { f.call_with_interpreter(&mut frame.interpreter, ctx) }
        } else {
            drop((ctx, frame_stack));
            return Ok(evm.frame_run()?);
        };

        let result = frame
            .process_next_action::<_, BenchError>(ctx, action)
            .inspect(|i| {
                if i.is_result() {
                    frame.set_finished(true);
                }
            })?;
        Ok(result)
    }
}

// ── Execution Helper ─────────────────────────────────────────────────────────

struct ExecResults {
    results: Vec<(bool, u64)>,
    dur: std::time::Duration,
}

fn execute_block(
    loader: &BinLoader,
    chain_id: Option<u64>,
    mut run_tx: impl FnMut(&mut BenchEvm) -> (bool, u64),
) -> ExecResults {
    let cfg = build_op_cfg(chain_id);
    let dummy_tx = OpTransaction::builder().build_fill();
    let mut evm: BenchEvm = {
        let ctx = OpCtx::<EmptyDB>::op()
            .with_cfg(cfg)
            .with_db(loader.build_cache_db())
            .with_tx(dummy_tx);
        op_revm::OpEvm::new(ctx, ())
    };

    let mut results = Vec::with_capacity(loader.tx_count());
    let t0 = Instant::now();

    for tx_bin in loader.raw_txs() {
        if tx_bin.tx_type == 0x7e {
            results.push((true, 0));
            continue;
        }
        evm.0.ctx.tx = build_op_tx(tx_bin);
        results.push(run_tx(&mut evm));
    }

    ExecResults {
        results,
        dur: t0.elapsed(),
    }
}

// ── Main ─────────────────────────────────────────────────────────────────────

fn main() {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/home/ubuntu/sipeng/bench_data".into());
    let bench_dir = Path::new(&dir);
    let block: u64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(38004930);
    let cache_dir = PathBuf::from("./jit_cache");

    println!("=== Skeleton Compilation Benchmark ===");
    println!("Block {block} | Cache: {}\n", cache_dir.display());

    // ── Phase 1: Load ────────────────────────────────────────────────────
    let loader = BinLoader::new(bench_dir, block).unwrap_or_else(|e| {
        eprintln!("Failed: {e}");
        std::process::exit(1);
    });
    println!(
        "Loaded {} accounts, {} codes, {} txs",
        loader.account_count(),
        loader.code_count(),
        loader.tx_count()
    );

    // ── Phase 2: Skeleton grouping ───────────────────────────────────────
    let all_codes: HashMap<B256, Vec<u8>> = loader
        .code_values()
        .iter()
        .filter(|(_, bc)| !bc.is_empty())
        .map(|(h, bc)| (*h, bc.original_byte_slice().to_vec()))
        .collect();

    let mut skeleton_groups_raw: HashMap<u64, Vec<(B256, Vec<u8>)>> = HashMap::new();
    for (hash, bytes) in &all_codes {
        let skel = extract_opcode_skeleton(bytes);
        let skel_hash = hash_bytes(&skel);
        skeleton_groups_raw
            .entry(skel_hash)
            .or_default()
            .push((*hash, bytes.clone()));
    }

    let mut dup_groups: Vec<_> = skeleton_groups_raw
        .into_iter()
        .filter(|(_, members)| members.len() >= 2)
        .collect();
    dup_groups.sort_by_key(|(_, m)| std::cmp::Reverse(m.len()));

    if dup_groups.is_empty() {
        println!("No duplicated skeletons found.");
        return;
    }

    // Analyze variance for each group
    struct AnalyzedGroup {
        skel_hash: u64,
        members: Vec<(B256, Vec<u8>)>,
        variance: SkeletonVariance,
    }
    let analyzed: Vec<AnalyzedGroup> = dup_groups
        .into_iter()
        .map(|(skel_hash, members)| {
            let refs: Vec<&[u8]> = members.iter().map(|(_, b)| b.as_slice()).collect();
            let variance = analyze_skeleton_group(&refs);
            AnalyzedGroup {
                skel_hash,
                members,
                variance,
            }
        })
        .collect();

    let skeleton_member_hashes: HashSet<B256> = analyzed
        .iter()
        .flat_map(|g| g.members.iter().map(|(h, _)| *h))
        .collect();
    let total_skeleton_members = skeleton_member_hashes.len();
    let n_groups = analyzed.len();

    println!(
        "\n{n_groups} skeleton groups, {total_skeleton_members} contracts (of {})",
        all_codes.len()
    );
    for g in &analyzed {
        println!(
            "  {:016x}: {} copies, {} variant",
            g.skel_hash,
            g.members.len(),
            g.variance.num_variant,
        );
    }

    // ── Phase 3: Compilation ────────────────────────────────────────────
    //
    // Both strategies share a common base of 254 non-skeleton per-hash
    // compilations. The only difference is how the 50 skeleton-member
    // contracts are handled:
    //   Strategy A: compile each of the 50 members per-hash
    //   Strategy B: compile 11 skeleton groups (shared functions + data tables)

    // Split code maps
    let base_code_map: HashMap<B256, Bytecode> = loader
        .code_values()
        .iter()
        .filter(|(h, _)| !skeleton_member_hashes.contains(*h))
        .map(|(h, bc)| (*h, bc.clone()))
        .collect();
    let member_code_map: HashMap<B256, Bytecode> = loader
        .code_values()
        .iter()
        .filter(|(h, _)| skeleton_member_hashes.contains(*h))
        .map(|(h, bc)| (*h, bc.clone()))
        .collect();

    // 3a. Shared base: compile 254 non-skeleton per-hash (with AOT cache)
    println!(
        "\n--- Shared base: {} non-skeleton per-hash (with cache) ---",
        base_code_map.len()
    );
    let t_base = Instant::now();
    let compiled_base = compile_all_contracts_with_cache(&base_code_map, OPT, &cache_dir);
    let dur_base = t_base.elapsed();
    println!(
        "  {:.3}s ({} functions)",
        dur_base.as_secs_f64(),
        compiled_base.functions.len()
    );

    // 3b. Strategy A delta: compile 50 skeleton-member per-hash (with AOT cache)
    println!(
        "\n--- Strategy A delta: {} skeleton-member per-hash (with cache) ---",
        member_code_map.len()
    );
    let t_da = Instant::now();
    let compiled_members = compile_all_contracts_with_cache(&member_code_map, OPT, &cache_dir);
    let dur_da = t_da.elapsed();
    println!(
        "  {:.3}s ({} functions)",
        dur_da.as_secs_f64(),
        compiled_members.functions.len()
    );

    // Merge base + members for per-hash execution
    let all_per_hash_fns: Arc<HashMap<B256, RawEvmCompilerFn>> = {
        let mut merged = (*compiled_base.functions).clone();
        merged.extend(compiled_members.functions.iter().map(|(h, f)| (*h, *f)));
        Arc::new(merged)
    };
    let non_skeleton_fns = compiled_base.functions.clone();

    // 3c. Strategy B delta: compile 11 skeleton groups (with AOT cache)
    println!(
        "\n--- Strategy B delta: {} skeleton groups (with cache) ---",
        n_groups
    );
    let t_db = Instant::now();

    let mut skeleton_dispatch: HashMap<B256, (EvmCompilerFn, Vec<u8>)> = HashMap::new();
    let mut skeleton_libs: Vec<libloading::Library> = Vec::new();
    let mut new_registry_groups: Vec<RegistryGroup> = Vec::new();

    // Phase 1: try loading all from cache, collect misses
    let mut cached: Vec<(usize, RawEvmCompilerFn)> = Vec::new();
    let mut to_compile: Vec<usize> = Vec::new();
    for (i, g) in analyzed.iter().enumerate() {
        let artifacts = skeleton_cache_artifacts(&cache_dir, g.skel_hash);
        match load_skeleton_cached(&artifacts) {
            Ok((f, lib)) => {
                skeleton_libs.push(lib);
                cached.push((i, f));
            }
            Err(_) => to_compile.push(i),
        }
    }
    println!(
        "  cache: {} hit, {} to compile",
        cached.len(),
        to_compile.len()
    );

    // Phase 2: compile all misses in parallel
    let compiled_skeletons: Vec<(usize, Result<(), String>)> =
        std::thread::scope(|s| {
            let handles: Vec<_> = to_compile
                .iter()
                .map(|&i| {
                    let g = &analyzed[i];
                    let artifacts = skeleton_cache_artifacts(&cache_dir, g.skel_hash);
                    s.spawn(move || {
                        let result = compile_skeleton_to_cache(
                            &g.members[0].1,
                            &g.variance,
                            &artifacts,
                        );
                        (i, result)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

    // Phase 3: load compiled results
    for (i, result) in compiled_skeletons {
        let g = &analyzed[i];
        let artifacts = skeleton_cache_artifacts(&cache_dir, g.skel_hash);
        match result {
            Ok(()) => match load_skeleton_cached(&artifacts) {
                Ok((f, lib)) => {
                    skeleton_libs.push(lib);
                    cached.push((i, f));
                }
                Err(e) => eprintln!("  WARN: skel {:016x} load failed: {e}", g.skel_hash),
            },
            Err(e) => eprintln!("  WARN: skel {:016x} compile failed: {e}", g.skel_hash),
        }
    }

    // Phase 4: build data tables for all loaded groups
    for (i, raw_fn) in &cached {
        let g = &analyzed[*i];
        let skel_fn = EvmCompilerFn::new(*raw_fn);
        let member_hashes: Vec<B256> = g.members.iter().map(|(h, _)| *h).collect();

        for (hash, bytes) in &g.members {
            let table = build_data_table(bytes, &g.variance);
            skeleton_dispatch.insert(*hash, (skel_fn, table.data));
        }

        new_registry_groups.push(RegistryGroup::from_analysis(
            g.skel_hash,
            &member_hashes,
            &g.variance,
        ));
    }

    save_registry(
        &cache_dir,
        &SkeletonRegistry {
            groups: new_registry_groups,
        },
    );

    let dur_db = t_db.elapsed();
    println!("  {:.3}s ({} groups)", dur_db.as_secs_f64(), n_groups);

    // Compilation summary — fair comparison of only the differing part
    println!("\n--- Compilation Summary ---");
    println!(
        "  Shared base:       {:.3}s  ({} non-skeleton per-hash)",
        dur_base.as_secs_f64(),
        compiled_base.functions.len()
    );
    println!(
        "  Strategy A delta:  {:.3}s  ({} skeleton-member per-hash)",
        dur_da.as_secs_f64(),
        compiled_members.functions.len()
    );
    println!(
        "  Strategy B delta:  {:.3}s  ({} skeleton groups → {} contracts)",
        dur_db.as_secs_f64(),
        n_groups,
        total_skeleton_members,
    );
    if dur_da.as_secs_f64() > 0.001 {
        println!(
            "  Speedup (A delta / B delta): {:.1}x",
            dur_da.as_secs_f64() / dur_db.as_secs_f64()
        );
    }
    println!(
        "  Total A: {:.3}s  |  Total B: {:.3}s",
        dur_base.as_secs_f64() + dur_da.as_secs_f64(),
        dur_base.as_secs_f64() + dur_db.as_secs_f64(),
    );

    // ── Phase 4: Execution ───────────────────────────────────────────────
    println!(
        "\n--- Executing block {block} ({} txs) ---",
        loader.tx_count()
    );
    let chain_id = loader.raw_txs().first().and_then(|tx| tx.chain_id);

    let native = execute_block(&loader, chain_id, |evm| {
        let mut h = NativeHandler;
        match h.run(evm) {
            Ok(r) => extract_gas(&r),
            Err(_) => (false, 0),
        }
    });

    let per_hash = execute_block(&loader, chain_id, |evm| {
        let mut h = JitHandler {
            functions: all_per_hash_fns.clone(),
        };
        match h.run(evm) {
            Ok(r) => extract_gas(&r),
            Err(_) => (false, 0),
        }
    });

    let fns_ns = non_skeleton_fns.clone();
    let sd = Arc::new(skeleton_dispatch);
    let skeleton = execute_block(&loader, chain_id, |evm| {
        let mut h = SkeletonHandler {
            per_hash_fns: fns_ns.clone(),
            skeleton_dispatch: sd.clone(),
        };
        match h.run(evm) {
            Ok(r) => extract_gas(&r),
            Err(_) => (false, 0),
        }
    });

    // ── Phase 5: Results ─────────────────────────────────────────────────
    let gas_n: u64 = native.results.iter().map(|(_, g)| g).sum();
    let gas_p: u64 = per_hash.results.iter().map(|(_, g)| g).sum();
    let gas_s: u64 = skeleton.results.iter().map(|(_, g)| g).sum();

    println!("\n=== Execution Results ===");
    println!(
        "  Native:       {:.6}s  gas={gas_n}",
        native.dur.as_secs_f64()
    );
    println!(
        "  Per-hash JIT: {:.6}s  gas={gas_p}",
        per_hash.dur.as_secs_f64()
    );
    println!(
        "  Skeleton JIT: {:.6}s  gas={gas_s}",
        skeleton.dur.as_secs_f64()
    );

    if native.dur.as_secs_f64() > 0.0 {
        println!(
            "\n  Speedup vs native: per-hash {:.2}x, skeleton {:.2}x",
            native.dur.as_secs_f64() / per_hash.dur.as_secs_f64(),
            native.dur.as_secs_f64() / skeleton.dur.as_secs_f64(),
        );
    }

    let mut mismatches = 0u32;
    for (i, (ph, sk)) in per_hash
        .results
        .iter()
        .zip(skeleton.results.iter())
        .enumerate()
    {
        if ph != sk {
            mismatches += 1;
            eprintln!(
                "  MISMATCH tx[{i}]: per-hash=({},{}gas) skeleton=({},{}gas)",
                ph.0, ph.1, sk.0, sk.1
            );
        }
    }

    println!("\n=== Correctness ===");
    println!(
        "  {}/{} match ({mismatches} mismatches)",
        per_hash.results.len() - mismatches as usize,
        per_hash.results.len()
    );
    println!(
        "  Gas: per-hash={gas_p} skeleton={gas_s} {}",
        if gas_p == gas_s { "EQUAL" } else { "DIFFER!" }
    );

    if mismatches == 0 {
        println!("\n  PASS");
    }

    println!("\nDone.");
}
