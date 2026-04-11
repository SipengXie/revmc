//! Stage B — SkelOT paper compile benchmark.

#[path = "../bin_common.rs"]
#[allow(dead_code)]
mod bin_common;

#[path = "../skelot_common.rs"]
#[allow(dead_code)]
mod skelot_common;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use clap::Parser;
use revm::bytecode::Bytecode;
use revm::primitives::{Address, B256, HashMap as RevmHashMap, U256};
use revm::state::AccountInfo;
use revmc::skeleton::{analyze_skeleton_group, SkeletonVariance};
use revmc::OptimizationLevel;
use serde::{Deserialize, Serialize};

use crate::skelot_common::{
    compile_per_hash, compile_skeleton_group, extract_opcode_skeleton, hash_bytes, per_hash_paths,
    should_skip_bytes, skeleton_paths,
};

#[derive(Parser)]
#[command(name = "skelot_compile_bench")]
struct Args {
    #[arg(long)]
    bench_dir: String,
    #[arg(long, default_value_t = 38004930)]
    start_block: u64,
    #[arg(long, default_value_t = 10000)]
    block_count: u64,
    #[arg(long, default_value = "both")]
    mode: String,
    #[arg(long, default_value = "block_38004930")]
    scope: String,
    #[arg(long)]
    cache_dir: String,
    #[arg(long, default_value_t = 8)]
    threads: usize,
    #[arg(long, default_value_t = false)]
    resume: bool,
    #[arg(long)]
    out: String,
}

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
struct FamilyRecord {
    skeleton_hash: String,
    members: usize,
    compile_seconds: f64,
    o_bytes: u64,
    fallback: bool,
}

#[derive(Serialize, Deserialize)]
struct CompileRecord {
    scope: String,
    mode: String,
    n_units: usize,
    total_compile_seconds: f64,
    total_o_bytes: u64,
    per_unit_ms: Vec<f64>,
    per_unit_o_bytes: Vec<u64>,
    compile_unit_hashes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    per_family: Option<Vec<FamilyRecord>>,
    fallback_count: usize,
}

#[derive(Serialize, Deserialize)]
struct CompileBenchOut {
    records: Vec<CompileRecord>,
}

fn collect_bytecodes(args: &Args) -> HashMap<B256, Vec<u8>> {
    let bench_dir = PathBuf::from(&args.bench_dir);
    let mut all: HashMap<B256, Vec<u8>> = HashMap::new();
    for bn in args.start_block..args.start_block + args.block_count {
        let path = bench_dir.join(format!("states/{bn}.bin"));
        let Ok(data) = fs::read(&path) else { continue };
        let Ok(snap): Result<CacheSnapshot, _> = bincode::deserialize(&data) else {
            continue;
        };
        for (hash, bytecode) in snap.codes {
            let raw = bytecode.original_byte_slice().to_vec();
            if should_skip_bytes(&raw) {
                continue;
            }
            all.entry(hash).or_insert(raw);
        }
    }
    all
}

fn run_per_hash(
    contracts: &HashMap<B256, Vec<u8>>,
    cache_dir: &Path,
    threads: usize,
    resume: bool,
) -> (Vec<(B256, f64, u64)>, f64) {
    let opt = OptimizationLevel::Aggressive;
    let mut jobs: Vec<(B256, Vec<u8>)> =
        contracts.iter().map(|(h, b)| (*h, b.clone())).collect();
    jobs.sort_by_key(|(_, b)| std::cmp::Reverse(b.len()));

    if !resume {
        for (hash, _) in &jobs {
            let artifacts = per_hash_paths(cache_dir, hash, opt);
            let _ = fs::remove_file(&artifacts.object);
            let _ = fs::remove_file(&artifacts.library);
        }
    }

    let compiled = std::sync::Mutex::new(Vec::<(B256, f64, u64)>::new());
    let total_t0 = Instant::now();
    let mut assignments: Vec<Vec<(B256, Vec<u8>)>> =
        (0..threads).map(|_| Vec::new()).collect();
    for (i, item) in jobs.into_iter().enumerate() {
        assignments[i % threads].push(item);
    }
    let counter = AtomicUsize::new(0);

    std::thread::scope(|s| {
        for chunk in assignments {
            let compiled_ref = &compiled;
            let counter_ref = &counter;
            s.spawn(move || {
                for (hash, bytes) in chunk {
                    let artifacts = per_hash_paths(cache_dir, &hash, opt);
                    if resume && artifacts.library.exists() {
                        let size = fs::metadata(&artifacts.object)
                            .map(|m| m.len())
                            .unwrap_or(0);
                        compiled_ref.lock().unwrap().push((hash, 0.0, size));
                        counter_ref.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    let t0 = Instant::now();
                    match compile_per_hash(
                        &bytes,
                        &artifacts.symbol,
                        &artifacts.object,
                        &artifacts.library,
                        opt,
                    ) {
                        Ok(()) => {
                            let dur = t0.elapsed().as_secs_f64();
                            let size = fs::metadata(&artifacts.object)
                                .map(|m| m.len())
                                .unwrap_or(0);
                            compiled_ref.lock().unwrap().push((hash, dur, size));
                            let done = counter_ref.fetch_add(1, Ordering::Relaxed) + 1;
                            if done % 50 == 0 {
                                eprintln!(
                                    "  [per-hash {done}] {:.1}s",
                                    total_t0.elapsed().as_secs_f64()
                                );
                            }
                        }
                        Err(e) => eprintln!("  FAIL 0x{}...: {e}", &hex::encode(hash)[..12]),
                    }
                }
            });
        }
    });

    let total_dur = total_t0.elapsed().as_secs_f64();
    (compiled.into_inner().unwrap(), total_dur)
}

fn run_skeleton(
    contracts: &HashMap<B256, Vec<u8>>,
    cache_dir: &Path,
    threads: usize,
    resume: bool,
) -> (Vec<(B256, f64, u64)>, Vec<FamilyRecord>, f64, usize) {
    let opt = OptimizationLevel::Aggressive;

    let mut groups_raw: HashMap<u64, Vec<(B256, Vec<u8>)>> = HashMap::new();
    for (hash, bytes) in contracts {
        let skel = extract_opcode_skeleton(bytes);
        let skel_hash = hash_bytes(&skel);
        groups_raw
            .entry(skel_hash)
            .or_default()
            .push((*hash, bytes.clone()));
    }

    let mut singletons: Vec<(B256, Vec<u8>)> = Vec::new();
    let mut multi: Vec<(u64, Vec<(B256, Vec<u8>)>)> = Vec::new();
    for (skel_hash, members) in groups_raw {
        if members.len() >= 2 {
            multi.push((skel_hash, members));
        } else {
            singletons.push(members.into_iter().next().unwrap());
        }
    }

    eprintln!(
        "  skeleton groups: {} families, {} singletons",
        multi.len(),
        singletons.len()
    );

    let variances: Vec<(u64, Vec<(B256, Vec<u8>)>, SkeletonVariance)> = multi
        .into_iter()
        .map(|(sh, members)| {
            let refs: Vec<&[u8]> = members.iter().map(|(_, b)| b.as_slice()).collect();
            let variance = analyze_skeleton_group(&refs);
            (sh, members, variance)
        })
        .collect();

    if !resume {
        for (sh, _, _) in &variances {
            let a = skeleton_paths(cache_dir, *sh, opt);
            let _ = fs::remove_file(&a.object);
            let _ = fs::remove_file(&a.library);
        }
        for (hash, _) in &singletons {
            let a = per_hash_paths(cache_dir, hash, opt);
            let _ = fs::remove_file(&a.object);
            let _ = fs::remove_file(&a.library);
        }
    }

    let compiled = std::sync::Mutex::new(Vec::<(B256, f64, u64)>::new());
    let families_out = std::sync::Mutex::new(Vec::<FamilyRecord>::new());
    let fallback_count = AtomicUsize::new(0);
    let total_t0 = Instant::now();

    let mut assignments: Vec<Vec<(u64, Vec<(B256, Vec<u8>)>, SkeletonVariance)>> =
        (0..threads).map(|_| Vec::new()).collect();
    for (i, item) in variances.into_iter().enumerate() {
        assignments[i % threads].push(item);
    }

    std::thread::scope(|s| {
        for chunk in assignments {
            let compiled_ref = &compiled;
            let families_ref = &families_out;
            let fb_ref = &fallback_count;
            s.spawn(move || {
                for (skel_hash, members, variance) in chunk {
                    let a = skeleton_paths(cache_dir, skel_hash, opt);
                    let rep = members[0].1.clone();
                    let t0 = Instant::now();
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        compile_skeleton_group(
                            &rep,
                            &variance,
                            &a.symbol,
                            &a.object,
                            &a.library,
                            opt,
                        )
                    }));
                    match result {
                        Ok(Ok(())) => {
                            let dur = t0.elapsed().as_secs_f64();
                            let o_size =
                                fs::metadata(&a.object).map(|m| m.len()).unwrap_or(0);
                            compiled_ref.lock().unwrap().push((members[0].0, dur, o_size));
                            families_ref.lock().unwrap().push(FamilyRecord {
                                skeleton_hash: format!("{:016x}", skel_hash),
                                members: members.len(),
                                compile_seconds: dur,
                                o_bytes: o_size,
                                fallback: false,
                            });
                        }
                        _ => {
                            eprintln!(
                                "  skel {:016x} failed, falling back to per-hash for {} members",
                                skel_hash,
                                members.len()
                            );
                            fb_ref.fetch_add(members.len(), Ordering::Relaxed);
                            for (hash, bytes) in &members {
                                let a = per_hash_paths(cache_dir, hash, opt);
                                let t1 = Instant::now();
                                if let Err(e) = compile_per_hash(
                                    bytes,
                                    &a.symbol,
                                    &a.object,
                                    &a.library,
                                    opt,
                                ) {
                                    eprintln!(
                                        "  fallback FAIL 0x{}...: {e}",
                                        &hex::encode(hash)[..12]
                                    );
                                    continue;
                                }
                                let dur = t1.elapsed().as_secs_f64();
                                let o_size =
                                    fs::metadata(&a.object).map(|m| m.len()).unwrap_or(0);
                                compiled_ref.lock().unwrap().push((*hash, dur, o_size));
                            }
                            families_ref.lock().unwrap().push(FamilyRecord {
                                skeleton_hash: format!("{:016x}", skel_hash),
                                members: members.len(),
                                compile_seconds: 0.0,
                                o_bytes: 0,
                                fallback: true,
                            });
                        }
                    }
                }
            });
        }
    });

    let mut assignments: Vec<Vec<(B256, Vec<u8>)>> =
        (0..threads).map(|_| Vec::new()).collect();
    for (i, item) in singletons.into_iter().enumerate() {
        assignments[i % threads].push(item);
    }
    std::thread::scope(|s| {
        for chunk in assignments {
            let compiled_ref = &compiled;
            s.spawn(move || {
                for (hash, bytes) in chunk {
                    let a = per_hash_paths(cache_dir, &hash, OptimizationLevel::Aggressive);
                    let t0 = Instant::now();
                    if let Err(e) = compile_per_hash(
                        &bytes,
                        &a.symbol,
                        &a.object,
                        &a.library,
                        OptimizationLevel::Aggressive,
                    ) {
                        eprintln!(
                            "  singleton FAIL 0x{}...: {e}",
                            &hex::encode(hash)[..12]
                        );
                        continue;
                    }
                    let dur = t0.elapsed().as_secs_f64();
                    let o_size = fs::metadata(&a.object).map(|m| m.len()).unwrap_or(0);
                    compiled_ref.lock().unwrap().push((hash, dur, o_size));
                }
            });
        }
    });

    let total_dur = total_t0.elapsed().as_secs_f64();
    let fallback_total = fallback_count.load(Ordering::Relaxed);
    (
        compiled.into_inner().unwrap(),
        families_out.into_inner().unwrap(),
        total_dur,
        fallback_total,
    )
}

fn main() {
    let args = Args::parse();
    let contracts = collect_bytecodes(&args);
    eprintln!("Collected {} compilable bytecodes", contracts.len());
    let cache_dir = PathBuf::from(&args.cache_dir);
    fs::create_dir_all(&cache_dir).expect("mk cache");
    let mut records: Vec<CompileRecord> = Vec::new();

    if args.mode == "per-hash" || args.mode == "both" {
        let ph_cache = cache_dir.join("per_hash");
        fs::create_dir_all(&ph_cache).ok();
        let (items, total) = run_per_hash(&contracts, &ph_cache, args.threads, args.resume);
        let total_o: u64 = items.iter().map(|(_, _, s)| *s).sum();
        records.push(CompileRecord {
            scope: args.scope.clone(),
            mode: "per_hash".into(),
            n_units: items.len(),
            total_compile_seconds: total,
            total_o_bytes: total_o,
            per_unit_ms: items.iter().map(|(_, d, _)| d * 1000.0).collect(),
            per_unit_o_bytes: items.iter().map(|(_, _, s)| *s).collect(),
            compile_unit_hashes: items
                .iter()
                .map(|(h, _, _)| format!("0x{}", hex::encode(h)))
                .collect(),
            per_family: None,
            fallback_count: 0,
        });
    }
    if args.mode == "skeleton" || args.mode == "both" {
        let sk_cache = cache_dir.join("skelot");
        fs::create_dir_all(&sk_cache).ok();
        let (items, families, total, fb) =
            run_skeleton(&contracts, &sk_cache, args.threads, args.resume);
        let total_o: u64 = items.iter().map(|(_, _, s)| *s).sum();
        records.push(CompileRecord {
            scope: args.scope.clone(),
            mode: "skelot".into(),
            n_units: items.len(),
            total_compile_seconds: total,
            total_o_bytes: total_o,
            per_unit_ms: items.iter().map(|(_, d, _)| d * 1000.0).collect(),
            per_unit_o_bytes: items.iter().map(|(_, _, s)| *s).collect(),
            compile_unit_hashes: items
                .iter()
                .map(|(h, _, _)| format!("0x{}", hex::encode(h)))
                .collect(),
            per_family: Some(families),
            fallback_count: fb,
        });
    }

    let out = CompileBenchOut { records };
    let out_path = PathBuf::from(&args.out);
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent).ok();
    }
    let merged = if out_path.exists() {
        let existing: CompileBenchOut =
            serde_json::from_slice(&fs::read(&out_path).expect("read out"))
                .expect("parse existing out");
        let mut all = existing.records;
        all.extend(out.records);
        CompileBenchOut { records: all }
    } else {
        out
    };
    fs::write(
        &out_path,
        serde_json::to_vec_pretty(&merged).expect("serialize"),
    )
    .expect("write compile_bench.json");
    eprintln!("wrote {}", out_path.display());
}
