//! Skeleton-aware batch pre-compiler.
//!
//! Scans all blocks, groups contracts by opcode skeleton, then compiles:
//!   - Singletons (1 member): per-hash AOT (same as `precompile`)
//!   - Groups (2+ members): skeleton AOT (one .so per group + registry)
//!
//! Supports resume: skips contracts/groups already in cache.
//!
//! Usage:
//!   cargo run -p revmc-examples-runner --bin skeleton_precompile --release -- \
//!     --cache-dir /tmp/jit_cache
//!   cargo run -p revmc-examples-runner --bin skeleton_precompile --release -- \
//!     --cache-dir /tmp/jit_cache --start 38004930 --count 9997 --threads 16

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
use revmc::emse::{bytecode_view, group_skeleton_families, SkeletonMode};
use revmc::skeleton::{analyze_skeleton_group, build_data_table, SkeletonVariance};
use revmc::{EvmCompiler, EvmLlvmBackend, OptimizationLevel};
use serde::{Deserialize, Serialize};

use op_revm::OpSpecId;
use revm::primitives::hardfork::SpecId;

// ── Spec Constants ──────────────────────────────────────────────────────────

const OP_SPEC: OpSpecId = OpSpecId::ISTHMUS;
const ETH_SPEC: SpecId = OP_SPEC.into_eth_spec();

// ── Data Types ──────────────────────────────────────────────────────────────

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

#[derive(Serialize, Deserialize)]
struct SkeletonRegistry {
    groups: Vec<RegistryGroup>,
}

#[derive(Clone, Serialize, Deserialize)]
struct RegistryGroup {
    skeleton_hash: u64,
    member_hashes: Vec<[u8; 32]>,
    num_variant: u32,
    num_members: u32,
    /// Serialized variance: -1 = Invariant, >=0 = Variant { table_index }.
    /// One entry per PUSH1..PUSH32 instruction.
    #[serde(default)]
    variance_map: Vec<i32>,
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn opt_tag(opt: OptimizationLevel) -> &'static str {
    match opt {
        OptimizationLevel::None => "o0",
        OptimizationLevel::Less => "o1",
        OptimizationLevel::Default => "o2",
        OptimizationLevel::Aggressive => "o3",
    }
}

// ── Per-hash AOT ────────────────────────────────────────────────────────────

fn per_hash_artifacts(cache_dir: &Path, hash: &B256, opt: OptimizationLevel) -> (String, PathBuf, PathBuf) {
    let hash_hex = hex::encode(hash);
    let spec_tag = format!("{ETH_SPEC:?}").to_lowercase();
    let tag = opt_tag(opt);
    let key = format!("{hash_hex}__{spec_tag}__{tag}");
    let stem = cache_dir.join(&key);
    (
        format!("c_{hash_hex}"),
        stem.with_extension("o"),
        stem.with_extension(std::env::consts::DLL_EXTENSION),
    )
}

fn compile_per_hash(
    bytecode: &[u8],
    symbol: &str,
    obj: &Path,
    lib: &Path,
    opt: OptimizationLevel,
) -> Result<(), String> {
    if let Some(parent) = obj.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let context = Box::leak(Box::new(revmc::llvm::inkwell::context::Context::create()));
    let backend = EvmLlvmBackend::new(context, true, opt)
        .map_err(|e| format!("backend: {e}"))?;
    let mut compiler = EvmCompiler::new(backend);
    compiler
        .translate(symbol, bytecode, ETH_SPEC)
        .map_err(|e| format!("translate: {e}"))?;
    compiler
        .write_object_to_file(obj)
        .map_err(|e| format!("write: {e}"))?;
    revmc::Linker::new()
        .link(lib, [obj])
        .map_err(|e| format!("link: {e}"))?;
    Ok(())
}

// ── Skeleton AOT ────────────────────────────────────────────────────────────

fn skeleton_artifacts(cache_dir: &Path, skel_hash: u64, opt: OptimizationLevel) -> (String, PathBuf, PathBuf) {
    let spec_tag = format!("{ETH_SPEC:?}").to_lowercase();
    let tag = opt_tag(opt);
    let key = format!("skel_{skel_hash:016x}__{spec_tag}__{tag}");
    let stem = cache_dir.join(&key);
    (
        format!("skel_{skel_hash:016x}"),
        stem.with_extension("o"),
        stem.with_extension(std::env::consts::DLL_EXTENSION),
    )
}

fn compile_skeleton(
    bytecode: &[u8],
    variance: &SkeletonVariance,
    symbol: &str,
    obj: &Path,
    lib: &Path,
    opt: OptimizationLevel,
) -> Result<(), String> {
    if let Some(parent) = obj.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let context = Box::leak(Box::new(revmc::llvm::inkwell::context::Context::create()));
    let backend = EvmLlvmBackend::new(context, true, opt)
        .map_err(|e| format!("backend: {e}"))?;
    let mut compiler = EvmCompiler::new(backend);
    compiler
        .translate_skeleton(symbol, bytecode, ETH_SPEC, variance)
        .map_err(|e| format!("translate_skeleton: {e}"))?;
    compiler
        .write_object_to_file(obj)
        .map_err(|e| format!("write: {e}"))?;
    revmc::Linker::new()
        .link(lib, [obj])
        .map_err(|e| format!("link: {e}"))?;
    Ok(())
}

// ── CLI ─────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "skeleton_precompile",
    about = "Skeleton-aware batch pre-compiler for all EVM contracts"
)]
struct Args {
    /// Path to bench_data directory
    #[arg(long, default_value = "/home/ubuntu/sipeng/bench_data_base")]
    dir: String,

    /// AOT cache output directory
    #[arg(long, default_value = "./jit_cache")]
    cache_dir: String,

    /// First block number
    #[arg(long, default_value_t = 38004930)]
    start: u64,

    /// Number of blocks to scan
    #[arg(long, default_value_t = 9997)]
    count: u64,

    /// Number of compilation threads (0 = auto-detect)
    #[arg(long, default_value_t = 0)]
    threads: usize,

    /// Optimization level: 0=None, 1=Less, 2=Default, 3=Aggressive
    #[arg(long, default_value_t = 3)]
    opt_level: u8,

    /// Family boundary: "current" (PUSH-immediates-only) or "emse" (also
    /// strips CBOR metadata + trailing zeros; admission guards apply).
    /// Use a separate --cache-dir per mode — registry and data-table file
    /// names are shared.
    #[arg(long, default_value = "current")]
    skeleton_mode: SkeletonMode,
}

// ── Main ────────────────────────────────────────────────────────────────────

fn main() {
    let args = Args::parse();
    let bench_dir = Path::new(&args.dir);
    let cache_dir = PathBuf::from(&args.cache_dir);
    let opt = match args.opt_level {
        0 => OptimizationLevel::None,
        1 => OptimizationLevel::Less,
        2 => OptimizationLevel::Default,
        _ => OptimizationLevel::Aggressive,
    };
    let n_threads = if args.threads == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    } else {
        args.threads
    };

    println!("=== Skeleton-Aware Batch Pre-compiler ===");
    println!("  Blocks: {}..{} ({} blocks)", args.start, args.start + args.count, args.count);
    println!("  Cache: {}", cache_dir.display());
    println!("  Threads: {n_threads}");
    println!("  Opt: {:?}\n", opt);

    // ── Phase 1: Scan blocks, collect unique bytecodes ──────────────────
    println!("--- Phase 1: Scanning blocks ---");
    let t0 = Instant::now();
    let mut all_codes: HashMap<B256, Vec<u8>> = HashMap::new();
    let mut scanned = 0u64;

    for bn in args.start..(args.start + args.count) {
        let path = bench_dir.join(format!("states/{bn}.bin"));
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let snapshot: CacheSnapshot = match bincode::deserialize(&data) {
            Ok(s) => s,
            Err(_) => continue,
        };
        for (hash, bytecode) in snapshot.codes {
            if !bytecode.is_empty() && bytecode.original_byte_slice().first() != Some(&0xEF) {
                all_codes
                    .entry(hash)
                    .or_insert_with(|| bytecode.original_byte_slice().to_vec());
            }
        }
        scanned += 1;
        if scanned % 1000 == 0 {
            eprintln!(
                "  {scanned} blocks, {} unique codes",
                all_codes.len()
            );
        }
    }
    println!(
        "  {scanned} blocks scanned in {:.1}s, {} unique non-empty bytecodes\n",
        t0.elapsed().as_secs_f64(),
        all_codes.len()
    );

    // ── Phase 2: Group by skeleton, classify ────────────────────────────
    println!("--- Phase 2: Skeleton grouping ---");
    let t1 = Instant::now();
    // Shared grouping: hash buckets are verified by full skeleton-byte
    // equality (colliding buckets split); under EMSE the g1/g2 admission
    // guards run here and rejected members fall back to per-hash.
    let grouping = group_skeleton_families(&all_codes, args.skeleton_mode);
    let mut singletons: Vec<(B256, Vec<u8>)> =
        grouping.per_hash.iter().map(|h| (*h, all_codes[h].clone())).collect();
    let groups: Vec<(u64, Vec<(B256, Vec<u8>)>)> = grouping
        .families
        .iter()
        .map(|f| (f.skel_hash, f.members.iter().map(|h| (*h, all_codes[h].clone())).collect()))
        .collect();

    let group_member_count: usize = groups.iter().map(|(_, m)| m.len()).sum();
    println!(
        "  {} per-hash units + {} groups ({} contracts) = {} total [mode={}]",
        singletons.len(),
        groups.len(),
        group_member_count,
        singletons.len() + group_member_count,
        args.skeleton_mode.as_str(),
    );
    println!(
        "  admission: g1 rejected {} skeletons ({} members), g2 rejected {} members, {} collision groups split",
        grouping.stats.g1_rejected_skeletons,
        grouping.stats.g1_rejected_members,
        grouping.stats.g2_rejected_members,
        grouping.stats.collision_groups_split,
    );
    for (i, (sh, m)) in groups.iter().enumerate().take(10) {
        println!("    {:016x}: {} copies", sh, m.len());
        if i == 9 && groups.len() > 10 {
            println!("    ... ({} more groups)", groups.len() - 10);
        }
    }

    // Analyze variance for each group over the mode's bytecode view.
    // Static-jump-conflicting families are degraded to per-hash, mirroring
    // the compile driver (their members compile as singletons instead).
    let group_variances: Vec<(u64, Vec<(B256, Vec<u8>)>, SkeletonVariance)> = groups
        .into_iter()
        .filter_map(|(skel_hash, members)| {
            let refs: Vec<&[u8]> =
                members.iter().map(|(_, b)| bytecode_view(b, args.skeleton_mode)).collect();
            let variance = analyze_skeleton_group(&refs);
            if revmc::skeleton::variance_conflicts_static_jumps(refs[0], &variance) {
                println!(
                    "  skel {skel_hash:016x}: {} members degraded to per-hash (variant PUSH feeds a static jump)",
                    members.len()
                );
                singletons.extend(members);
                return None;
            }
            Some((skel_hash, members, variance))
        })
        .collect();

    println!("  Grouped in {:.1}s\n", t1.elapsed().as_secs_f64());

    // ── Phase 3: Filter cached, prepare compile lists ───────────────────
    std::fs::create_dir_all(&cache_dir).ok();

    // Singletons: check per-hash cache
    let mut singleton_to_compile: Vec<(B256, Vec<u8>)> = Vec::new();
    let mut singleton_cached = 0usize;
    for (hash, bytes) in &singletons {
        let (_, _, lib) = per_hash_artifacts(&cache_dir, hash, opt);
        if lib.exists() {
            singleton_cached += 1;
        } else {
            singleton_to_compile.push((*hash, bytes.clone()));
        }
    }
    // Sort by size descending for better load balancing
    singleton_to_compile.sort_by_key(|(_, b)| std::cmp::Reverse(b.len()));

    // Groups: check skeleton cache
    let mut group_to_compile: Vec<(u64, Vec<u8>, SkeletonVariance)> = Vec::new();
    let mut group_cached = 0usize;
    let mut registry_groups: Vec<RegistryGroup> = Vec::new();

    for (skel_hash, members, variance) in &group_variances {
        let (_, _, lib) = skeleton_artifacts(&cache_dir, *skel_hash, opt);
        if lib.exists() {
            group_cached += 1;
        } else {
            group_to_compile.push((
                *skel_hash,
                bytecode_view(&members[0].1, args.skeleton_mode).to_vec(),
                variance.clone(),
            ));
        }
        let variance_map: Vec<i32> = variance.pushes.iter().map(|p| {
            match p {
                revmc::skeleton::PushClassification::Invariant => -1,
                revmc::skeleton::PushClassification::Variant { table_index } => *table_index as i32,
            }
        }).collect();
        registry_groups.push(RegistryGroup {
            skeleton_hash: *skel_hash,
            member_hashes: members.iter().map(|(h, _)| h.0).collect(),
            num_variant: variance.num_variant,
            num_members: members.len() as u32,
            variance_map,
        });
    }
    // Sort by bytecode size descending
    group_to_compile.sort_by_key(|(_, b, _)| std::cmp::Reverse(b.len()));

    println!("--- Phase 3: Compiling ---");
    println!(
        "  Singletons: {} cached, {} to compile",
        singleton_cached,
        singleton_to_compile.len()
    );
    println!(
        "  Skeleton groups: {} cached, {} to compile",
        group_cached,
        group_to_compile.len()
    );
    let total_to_compile = singleton_to_compile.len() + group_to_compile.len();
    println!("  Total: {total_to_compile} compilations with {n_threads} threads\n");

    if total_to_compile == 0 {
        println!("Nothing to compile. All contracts cached.");
        save_registry(&cache_dir, &registry_groups);
        return;
    }

    // ── Phase 4: Parallel compilation (singletons + groups together) ────
    // Merge into a unified work list for maximum thread utilization.
    enum CompileJob {
        Singleton(B256, Vec<u8>),
        Skeleton(u64, Vec<u8>, SkeletonVariance),
    }

    let mut jobs: Vec<CompileJob> = Vec::with_capacity(total_to_compile);
    // Interleave by size: both lists are sorted by size desc.
    // Merge them to keep the largest jobs first for better load balancing.
    let mut si = 0;
    let mut gi = 0;
    while si < singleton_to_compile.len() || gi < group_to_compile.len() {
        let s_size = singleton_to_compile.get(si).map(|(_, b)| b.len()).unwrap_or(0);
        let g_size = group_to_compile.get(gi).map(|(_, b, _)| b.len()).unwrap_or(0);
        if s_size >= g_size && si < singleton_to_compile.len() {
            let (h, b) = singleton_to_compile[si].clone();
            jobs.push(CompileJob::Singleton(h, b));
            si += 1;
        } else if gi < group_to_compile.len() {
            let (sh, b, v) = group_to_compile[gi].clone();
            jobs.push(CompileJob::Skeleton(sh, b, v));
            gi += 1;
        } else {
            break;
        }
    }

    // Round-robin assignment to threads
    let mut assignments: Vec<Vec<CompileJob>> = (0..n_threads).map(|_| Vec::new()).collect();
    for (i, job) in jobs.into_iter().enumerate() {
        assignments[i % n_threads].push(job);
    }

    let compile_start = Instant::now();
    let compiled_count = AtomicUsize::new(0);
    let failed_count = AtomicUsize::new(0);

    std::thread::scope(|s| {
        for (tid, chunk) in assignments.into_iter().enumerate() {
            let compiled_ref = &compiled_count;
            let failed_ref = &failed_count;
            let cache_dir = &cache_dir;
            s.spawn(move || {
                for job in &chunk {
                    let result = match job {
                        CompileJob::Singleton(hash, bytes) => {
                            let (sym, obj, lib) = per_hash_artifacts(cache_dir, hash, opt);
                            compile_per_hash(bytes, &sym, &obj, &lib, opt)
                        }
                        CompileJob::Skeleton(skel_hash, bytes, variance) => {
                            let (sym, obj, lib) = skeleton_artifacts(cache_dir, *skel_hash, opt);
                            compile_skeleton(bytes, variance, &sym, &obj, &lib, opt)
                        }
                    };
                    match result {
                        Ok(()) => {
                            let done = compiled_ref.fetch_add(1, Ordering::Relaxed) + 1;
                            if done % 50 == 0 || done == total_to_compile {
                                let elapsed = compile_start.elapsed().as_secs_f64();
                                let rate = done as f64 / elapsed;
                                let remaining = (total_to_compile - done) as f64 / rate;
                                let eta_m = remaining / 60.0;
                                eprintln!(
                                    "  [{done}/{total_to_compile}] {:.1}s, {:.2}/s, ETA {:.0}m",
                                    elapsed, rate, eta_m
                                );
                            }
                        }
                        Err(e) => {
                            failed_ref.fetch_add(1, Ordering::Relaxed);
                            let tag = match job {
                                CompileJob::Singleton(h, _) => format!("hash:{}", &hex::encode(h)[..12]),
                                CompileJob::Skeleton(sh, _, _) => format!("skel:{sh:016x}"),
                            };
                            eprintln!("  [T{tid}] FAIL {tag}: {e}");
                        }
                    }
                }
            });
        }
    });

    let compile_dur = compile_start.elapsed();
    let compiled = compiled_count.load(Ordering::Relaxed);
    let failed = failed_count.load(Ordering::Relaxed);

    // ── Phase 5: Save registry ──────────────────────────────────────────
    save_registry(&cache_dir, &registry_groups);

    // ── Phase 5b: Save pre-serialized data tables ──────────────────────
    save_data_tables(&cache_dir, &group_variances, args.skeleton_mode);

    // ── Summary ─────────────────────────────────────────────────────────
    println!("\n=== Summary ===");
    println!("  Singletons: {} compiled, {} cached", singleton_to_compile.len().saturating_sub(failed), singleton_cached);
    println!("  Skeleton groups: {} compiled, {} cached", group_to_compile.len(), group_cached);
    println!("  Failed: {failed}");
    println!(
        "  Time: {:.1}s ({:.2} compilations/s)",
        compile_dur.as_secs_f64(),
        compiled as f64 / compile_dur.as_secs_f64().max(0.001)
    );
    println!(
        "  Total cached: {} singletons + {} groups ({} contracts)",
        singletons.len(),
        group_variances.len(),
        singletons.len() + group_member_count,
    );
}

fn save_data_tables(
    cache_dir: &Path,
    group_variances: &[(u64, Vec<(B256, Vec<u8>)>, SkeletonVariance)],
    skeleton_mode: SkeletonMode,
) {
    let mut data_tables: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
    for (_skel_hash, members, variance) in group_variances {
        for (code_hash, bytecode) in members {
            let table = build_data_table(bytecode_view(bytecode, skeleton_mode), variance);
            data_tables.insert(code_hash.0, table.data);
        }
    }
    let path = cache_dir.join("data_tables.bin");
    let data = bincode::serialize(&data_tables).expect("serialize data_tables");
    std::fs::write(&path, &data).expect("write data_tables");
    println!(
        "  Data tables saved: {} entries ({:.1} KB) → {}",
        data_tables.len(),
        data.len() as f64 / 1024.0,
        path.display()
    );
}

fn save_registry(cache_dir: &Path, groups: &[RegistryGroup]) {
    let registry = SkeletonRegistry {
        groups: groups.to_vec(),
    };
    let path = cache_dir.join("skeleton_registry.bin");
    let data = bincode::serialize(&registry).expect("serialize registry");
    std::fs::write(&path, data).expect("write registry");
    println!("  Registry saved: {} groups → {}", groups.len(), path.display());
}
