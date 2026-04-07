//! Compile EVM contracts from raw bytecode hex to AOT cache (.so).
//!
//! Supports single contract, batch file (one hex per line), and skeleton grouping.
//!
//! Usage:
//!   # Single contract
//!   compile_hex --cache-dir ./revmc_cache --bytecode 6080604052...
//!
//!   # Batch from file (one hex per line, optional 0x prefix)
//!   compile_hex --cache-dir ./revmc_cache --file contracts.hex
//!
//!   # Batch from stdin
//!   cat contracts.hex | compile_hex --cache-dir ./revmc_cache --stdin
//!
//!   # Enable skeleton grouping (groups structurally similar contracts)
//!   compile_hex --cache-dir ./revmc_cache --file contracts.hex --skeleton

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use clap::Parser;
use revm::primitives::hardfork::SpecId;
use revm::primitives::keccak256;
use revm::primitives::B256;
use revmc::skeleton::{analyze_skeleton_group, PushClassification, SkeletonVariance};
use revmc::{EvmCompiler, EvmLlvmBackend, OptimizationLevel};
use serde::{Deserialize, Serialize};

use op_revm::OpSpecId;

const OP_SPEC: OpSpecId = OpSpecId::ISTHMUS;
const ETH_SPEC: SpecId = OP_SPEC.into_eth_spec();

// ── Registry format (compatible with skeleton_precompile) ──────────────────

#[derive(Serialize, Deserialize)]
struct RegistryGroup {
    skeleton_hash: u64,
    member_hashes: Vec<[u8; 32]>,
    num_variant: u32,
    num_members: u32,
    #[serde(default)]
    variance_map: Vec<i32>,
}

#[derive(Serialize, Deserialize)]
struct SkeletonRegistry {
    groups: Vec<RegistryGroup>,
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn spec_tag() -> String {
    format!("{ETH_SPEC:?}").to_lowercase()
}

fn opt_tag(opt: OptimizationLevel) -> &'static str {
    match opt {
        OptimizationLevel::None => "o0",
        OptimizationLevel::Less => "o1",
        OptimizationLevel::Default => "o2",
        OptimizationLevel::Aggressive => "o3",
    }
}

fn per_hash_paths(cache_dir: &Path, hash: &B256, opt: OptimizationLevel) -> (String, PathBuf, PathBuf) {
    let hash_hex = hex::encode(hash);
    let key = format!("{hash_hex}__{}__{}", spec_tag(), opt_tag(opt));
    let stem = cache_dir.join(&key);
    (format!("c_{hash_hex}"), stem.with_extension("o"), stem.with_extension("so"))
}

fn skeleton_paths(cache_dir: &Path, skel_hash: u64, opt: OptimizationLevel) -> (String, PathBuf, PathBuf) {
    let key = format!("skel_{skel_hash:016x}__{}__{}", spec_tag(), opt_tag(opt));
    let stem = cache_dir.join(&key);
    (format!("skel_{skel_hash:016x}"), stem.with_extension("o"), stem.with_extension("so"))
}

fn extract_opcode_skeleton(bytes: &[u8]) -> Vec<u8> {
    let mut skeleton = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let op = bytes[i];
        skeleton.push(op);
        i += 1;
        if (0x60..=0x7f).contains(&op) {
            i += (op - 0x5f) as usize;
        }
    }
    skeleton
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn compile_per_hash(bytecode: &[u8], symbol: &str, obj: &Path, lib: &Path, opt: OptimizationLevel) -> Result<(), String> {
    if let Some(parent) = obj.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let context = Box::leak(Box::new(revmc::llvm::inkwell::context::Context::create()));
    let backend = EvmLlvmBackend::new(context, true, opt).map_err(|e| format!("backend: {e}"))?;
    let mut compiler = EvmCompiler::new(backend);
    compiler.translate(symbol, bytecode, ETH_SPEC).map_err(|e| format!("translate: {e}"))?;
    compiler.write_object_to_file(obj).map_err(|e| format!("write: {e}"))?;
    revmc::Linker::new().link(lib, [obj]).map_err(|e| format!("link: {e}"))?;
    Ok(())
}

fn compile_skeleton_group(
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
    let backend = EvmLlvmBackend::new(context, true, opt).map_err(|e| format!("backend: {e}"))?;
    let mut compiler = EvmCompiler::new(backend);
    compiler.translate_skeleton(symbol, bytecode, ETH_SPEC, variance).map_err(|e| format!("translate_skeleton: {e}"))?;
    compiler.write_object_to_file(obj).map_err(|e| format!("write: {e}"))?;
    revmc::Linker::new().link(lib, [obj]).map_err(|e| format!("link: {e}"))?;
    Ok(())
}

// ── Input parsing ──────────────────────────────────────────────────────────

fn parse_hex_line(line: &str) -> Option<Vec<u8>> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let hex_str = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    hex::decode(hex_str).ok()
}

fn read_bytecodes(args: &Args) -> Vec<Vec<u8>> {
    if let Some(ref hex) = args.bytecode {
        let hex_str = hex.strip_prefix("0x").unwrap_or(hex);
        let bytes = hex::decode(hex_str).unwrap_or_else(|e| {
            eprintln!("Error: invalid hex: {e}");
            std::process::exit(1);
        });
        return vec![bytes];
    }

    let content = if let Some(ref path) = args.file {
        std::fs::read_to_string(path).unwrap_or_else(|e| {
            eprintln!("Error: failed to read {path}: {e}");
            std::process::exit(1);
        })
    } else if args.stdin {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf).expect("failed to read stdin");
        buf
    } else {
        eprintln!("Error: provide --bytecode, --file, or --stdin");
        std::process::exit(1);
    };

    content.lines().filter_map(parse_hex_line).collect()
}

// ── CLI ────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "compile_hex", about = "Compile EVM bytecode hex to AOT .so (batch + skeleton)")]
struct Args {
    /// AOT cache output directory
    #[arg(long)]
    cache_dir: String,

    /// Single bytecode hex string (without 0x prefix)
    #[arg(long)]
    bytecode: Option<String>,

    /// File with one bytecode hex per line
    #[arg(long)]
    file: Option<String>,

    /// Read bytecode hex lines from stdin
    #[arg(long, default_value_t = false)]
    stdin: bool,

    /// Enable skeleton grouping: structurally similar contracts share one .so
    #[arg(long, default_value_t = false)]
    skeleton: bool,

    /// Optimization level: 0=None, 1=Less, 2=Default, 3=Aggressive
    #[arg(long, default_value_t = 3)]
    opt_level: u8,

    /// Number of compilation threads (0 = auto-detect)
    #[arg(long, default_value_t = 0)]
    threads: usize,
}

// ── Main ───────────────────────────────────────────────────────────────────

fn main() {
    let args = Args::parse();

    let raw_bytecodes = read_bytecodes(&args);
    if raw_bytecodes.is_empty() {
        eprintln!("Error: no valid bytecodes found");
        std::process::exit(1);
    }

    let opt = match args.opt_level {
        0 => OptimizationLevel::None,
        1 => OptimizationLevel::Less,
        2 => OptimizationLevel::Default,
        _ => OptimizationLevel::Aggressive,
    };
    let n_threads = if args.threads == 0 {
        std::thread::available_parallelism().map(|n| n.get().min(16)).unwrap_or(4)
    } else {
        args.threads
    };
    let cache_dir = PathBuf::from(&args.cache_dir);
    std::fs::create_dir_all(&cache_dir).ok();

    // Deduplicate by code_hash
    let mut contracts: HashMap<B256, Vec<u8>> = HashMap::new();
    for bytes in raw_bytecodes {
        if bytes.is_empty() {
            continue;
        }
        let hash = keccak256(&bytes);
        contracts.entry(hash).or_insert(bytes);
    }

    println!("=== compile_hex ===");
    println!("  {} unique contracts", contracts.len());
    println!("  skeleton: {}", args.skeleton);
    println!("  opt: {}", opt_tag(opt));
    println!("  threads: {n_threads}");
    println!("  cache: {}\n", cache_dir.display());

    if !args.skeleton {
        compile_all_per_hash(&cache_dir, &contracts, opt, n_threads);
    } else {
        compile_with_skeleton(&cache_dir, &contracts, opt, n_threads);
    }
}

// ── Per-hash only mode ─────────────────────────────────────────────────────

fn compile_all_per_hash(
    cache_dir: &Path,
    contracts: &HashMap<B256, Vec<u8>>,
    opt: OptimizationLevel,
    n_threads: usize,
) {
    let mut to_compile: Vec<(B256, Vec<u8>)> = Vec::new();
    let mut cached = 0usize;

    for (hash, bytes) in contracts {
        let (_, _, lib) = per_hash_paths(cache_dir, hash, opt);
        if lib.exists() {
            cached += 1;
        } else {
            to_compile.push((*hash, bytes.clone()));
        }
    }
    to_compile.sort_by_key(|(_, b)| std::cmp::Reverse(b.len()));

    println!("  Already cached: {cached}");
    println!("  To compile: {}\n", to_compile.len());

    if to_compile.is_empty() {
        println!("Nothing to compile.");
        return;
    }

    let total = to_compile.len();
    let compiled_count = AtomicUsize::new(0);
    let failed_count = AtomicUsize::new(0);
    let t0 = Instant::now();

    let mut assignments: Vec<Vec<(B256, Vec<u8>)>> = (0..n_threads).map(|_| Vec::new()).collect();
    for (i, item) in to_compile.into_iter().enumerate() {
        assignments[i % n_threads].push(item);
    }

    std::thread::scope(|s| {
        for (tid, chunk) in assignments.into_iter().enumerate() {
            let compiled_ref = &compiled_count;
            let failed_ref = &failed_count;
            s.spawn(move || {
                for (hash, bytes) in &chunk {
                    let (sym, obj, lib) = per_hash_paths(cache_dir, hash, opt);
                    match compile_per_hash(bytes, &sym, &obj, &lib, opt) {
                        Ok(()) => {
                            let done = compiled_ref.fetch_add(1, Ordering::Relaxed) + 1;
                            if done % 50 == 0 || done == total {
                                eprintln!("  [{done}/{total}] {:.1}s", t0.elapsed().as_secs_f64());
                            }
                        }
                        Err(e) => {
                            failed_ref.fetch_add(1, Ordering::Relaxed);
                            eprintln!("  [T{tid}] FAIL 0x{}...: {e}", &hex::encode(hash)[..12]);
                        }
                    }
                }
            });
        }
    });

    let compiled = compiled_count.load(Ordering::Relaxed);
    let failed = failed_count.load(Ordering::Relaxed);
    println!("\nDone: {compiled} compiled, {failed} failed, {cached} cached ({:.1}s)", t0.elapsed().as_secs_f64());
}

// ── Skeleton-aware mode ────────────────────────────────────────────────────

fn compile_with_skeleton(
    cache_dir: &Path,
    contracts: &HashMap<B256, Vec<u8>>,
    opt: OptimizationLevel,
    n_threads: usize,
) {
    // Group by opcode skeleton
    let mut skeleton_groups: HashMap<u64, Vec<(B256, Vec<u8>)>> = HashMap::new();
    for (hash, bytes) in contracts {
        let skel = extract_opcode_skeleton(bytes);
        let skel_hash = hash_bytes(&skel);
        skeleton_groups.entry(skel_hash).or_default().push((*hash, bytes.clone()));
    }

    let mut singletons: Vec<(B256, Vec<u8>)> = Vec::new();
    let mut groups: Vec<(u64, Vec<(B256, Vec<u8>)>)> = Vec::new();
    for (skel_hash, members) in skeleton_groups {
        if members.len() >= 2 {
            groups.push((skel_hash, members));
        } else {
            singletons.push(members.into_iter().next().unwrap());
        }
    }
    groups.sort_by_key(|(_, m)| std::cmp::Reverse(m.len()));

    let group_member_count: usize = groups.iter().map(|(_, m)| m.len()).sum();
    println!("--- Skeleton grouping ---");
    println!("  {} singletons (per-hash compile)", singletons.len());
    println!("  {} groups ({} contracts, skeleton compile)", groups.len(), group_member_count);
    for (i, (sh, m)) in groups.iter().enumerate().take(5) {
        println!("    skel_{sh:016x}: {} members", m.len());
        if i == 4 && groups.len() > 5 {
            println!("    ... ({} more groups)", groups.len() - 5);
        }
    }

    // Analyze variance
    let group_variances: Vec<(u64, Vec<(B256, Vec<u8>)>, SkeletonVariance)> = groups
        .into_iter()
        .map(|(skel_hash, members)| {
            let refs: Vec<&[u8]> = members.iter().map(|(_, b)| b.as_slice()).collect();
            let variance = analyze_skeleton_group(&refs);
            (skel_hash, members, variance)
        })
        .collect();

    // Filter cached
    let mut singleton_to_compile: Vec<(B256, Vec<u8>)> = Vec::new();
    let mut singleton_cached = 0usize;
    for (hash, bytes) in &singletons {
        let (_, _, lib) = per_hash_paths(cache_dir, hash, opt);
        if lib.exists() {
            singleton_cached += 1;
        } else {
            singleton_to_compile.push((*hash, bytes.clone()));
        }
    }
    singleton_to_compile.sort_by_key(|(_, b)| std::cmp::Reverse(b.len()));

    // Skeleton groups: carry all members so we can fallback to per-hash on failure
    let mut skel_to_compile: Vec<(u64, Vec<(B256, Vec<u8>)>, SkeletonVariance)> = Vec::new();
    let mut skel_cached = 0usize;
    for (skel_hash, members, variance) in &group_variances {
        let (_, _, lib) = skeleton_paths(cache_dir, *skel_hash, opt);
        if lib.exists() {
            skel_cached += 1;
        } else {
            skel_to_compile.push((*skel_hash, members.clone(), variance.clone()));
        }
    }
    skel_to_compile.sort_by_key(|(_, m, _)| std::cmp::Reverse(m[0].1.len()));

    let total = singleton_to_compile.len() + skel_to_compile.len();
    println!("\n--- Compiling ---");
    println!("  Singletons: {} cached, {} to compile", singleton_cached, singleton_to_compile.len());
    println!("  Skeleton:   {} cached, {} to compile", skel_cached, skel_to_compile.len());
    println!("  Total: {total} compilations\n");

    // Track which skeleton groups failed and need per-hash fallback
    let fallback_needed: std::sync::Mutex<Vec<Vec<(B256, Vec<u8>)>>> =
        std::sync::Mutex::new(Vec::new());

    if total > 0 {
        enum Job {
            Single(B256, Vec<u8>),
            Skeleton(u64, Vec<(B256, Vec<u8>)>, SkeletonVariance),
        }

        let mut jobs: Vec<Job> = Vec::with_capacity(total);
        let mut si = 0;
        let mut gi = 0;
        while si < singleton_to_compile.len() || gi < skel_to_compile.len() {
            let s_size = singleton_to_compile.get(si).map(|(_, b)| b.len()).unwrap_or(0);
            let g_size = skel_to_compile.get(gi).map(|(_, m, _)| m[0].1.len()).unwrap_or(0);
            if s_size >= g_size && si < singleton_to_compile.len() {
                let (h, b) = singleton_to_compile[si].clone();
                jobs.push(Job::Single(h, b));
                si += 1;
            } else if gi < skel_to_compile.len() {
                let (sh, m, v) = skel_to_compile[gi].clone();
                jobs.push(Job::Skeleton(sh, m, v));
                gi += 1;
            } else {
                break;
            }
        }

        let mut assignments: Vec<Vec<Job>> = (0..n_threads).map(|_| Vec::new()).collect();
        for (i, job) in jobs.into_iter().enumerate() {
            assignments[i % n_threads].push(job);
        }

        let compiled_count = AtomicUsize::new(0);
        let failed_count = AtomicUsize::new(0);
        let fallback_count = AtomicUsize::new(0);
        let t0 = Instant::now();

        std::thread::scope(|s| {
            for (tid, chunk) in assignments.into_iter().enumerate() {
                let compiled_ref = &compiled_count;
                let failed_ref = &failed_count;
                let fallback_ref = &fallback_count;
                let fallback_needed_ref = &fallback_needed;
                s.spawn(move || {
                    for job in chunk {
                        match job {
                            Job::Single(hash, bytes) => {
                                let (sym, obj, lib) = per_hash_paths(cache_dir, &hash, opt);
                                match compile_per_hash(&bytes, &sym, &obj, &lib, opt) {
                                    Ok(()) => { compiled_ref.fetch_add(1, Ordering::Relaxed); }
                                    Err(e) => {
                                        failed_ref.fetch_add(1, Ordering::Relaxed);
                                        eprintln!("  [T{tid}] FAIL 0x{}...: {e}", &hex::encode(hash)[..12]);
                                    }
                                }
                            }
                            Job::Skeleton(skel_hash, members, variance) => {
                                let (sym, obj, lib) = skeleton_paths(cache_dir, skel_hash, opt);
                                let representative = members[0].1.clone();
                                // catch_unwind: skeleton compiler may panic on certain bytecodes
                                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    compile_skeleton_group(&representative, &variance, &sym, &obj, &lib, opt)
                                }));
                                match result {
                                    Ok(Ok(())) => {
                                        compiled_ref.fetch_add(1, Ordering::Relaxed);
                                    }
                                    Ok(Err(e)) => {
                                        eprintln!("  [T{tid}] skel_{skel_hash:016x} failed ({} members), falling back to per-hash: {e}", members.len());
                                        fallback_ref.fetch_add(members.len(), Ordering::Relaxed);
                                        fallback_needed_ref.lock().unwrap().push(members);
                                    }
                                    Err(_) => {
                                        eprintln!("  [T{tid}] skel_{skel_hash:016x} panicked ({} members), falling back to per-hash", members.len());
                                        fallback_ref.fetch_add(members.len(), Ordering::Relaxed);
                                        fallback_needed_ref.lock().unwrap().push(members);
                                    }
                                }
                            }
                        }
                    }
                });
            }
        });

        let compiled = compiled_count.load(Ordering::Relaxed);
        let failed = failed_count.load(Ordering::Relaxed);
        let fallback = fallback_count.load(Ordering::Relaxed);
        println!("  Compiled: {compiled}, Failed: {failed} ({:.1}s)", t0.elapsed().as_secs_f64());

        // Fallback: compile failed skeleton members as per-hash
        let fallback_groups = fallback_needed.into_inner().unwrap();
        if !fallback_groups.is_empty() {
            let mut fb_items: Vec<(B256, Vec<u8>)> = fallback_groups.into_iter().flatten().collect();
            fb_items.sort_by_key(|(_, b)| std::cmp::Reverse(b.len()));

            println!("\n--- Fallback: per-hash compile for {fallback} contracts ---");
            let fb_compiled = AtomicUsize::new(0);
            let fb_failed = AtomicUsize::new(0);
            let fb_t0 = Instant::now();

            let mut fb_assign: Vec<Vec<(B256, Vec<u8>)>> = (0..n_threads).map(|_| Vec::new()).collect();
            for (i, item) in fb_items.into_iter().enumerate() {
                fb_assign[i % n_threads].push(item);
            }

            std::thread::scope(|s| {
                for (tid, chunk) in fb_assign.into_iter().enumerate() {
                    let compiled_ref = &fb_compiled;
                    let failed_ref = &fb_failed;
                    s.spawn(move || {
                        for (hash, bytes) in &chunk {
                            let (sym, obj, lib) = per_hash_paths(cache_dir, hash, opt);
                            if lib.exists() {
                                compiled_ref.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                            match compile_per_hash(bytes, &sym, &obj, &lib, opt) {
                                Ok(()) => { compiled_ref.fetch_add(1, Ordering::Relaxed); }
                                Err(e) => {
                                    failed_ref.fetch_add(1, Ordering::Relaxed);
                                    eprintln!("  [T{tid}] FAIL 0x{}...: {e}", &hex::encode(hash)[..12]);
                                }
                            }
                        }
                    });
                }
            });

            let fb_ok = fb_compiled.load(Ordering::Relaxed);
            let fb_err = fb_failed.load(Ordering::Relaxed);
            println!("  Fallback: {fb_ok} compiled, {fb_err} failed ({:.1}s)", fb_t0.elapsed().as_secs_f64());
        }
    }

    // Save registry (only groups whose .so actually exists)
    let registry_groups: Vec<RegistryGroup> = group_variances
        .iter()
        .filter(|(skel_hash, _, _)| {
            let (_, _, lib) = skeleton_paths(cache_dir, *skel_hash, opt);
            lib.exists()
        })
        .map(|(skel_hash, members, variance)| {
            let variance_map: Vec<i32> = variance.pushes.iter().map(|p| match p {
                PushClassification::Invariant => -1,
                PushClassification::Variant { table_index } => *table_index as i32,
            }).collect();
            RegistryGroup {
                skeleton_hash: *skel_hash,
                member_hashes: members.iter().map(|(h, _)| h.0).collect(),
                num_variant: variance.num_variant,
                num_members: members.len() as u32,
                variance_map,
            }
        })
        .collect();

    if !registry_groups.is_empty() {
        let registry = SkeletonRegistry { groups: registry_groups };
        let path = cache_dir.join("skeleton_registry.bin");
        let data = bincode::serialize(&registry).expect("serialize registry");
        std::fs::write(&path, &data).expect("write registry");
        println!("\n  Registry saved: {} groups -> {}", registry.groups.len(), path.display());
    }

    println!("\nDone.");
}
