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
use revm::primitives::B256;
use revmc::skeleton::{analyze_skeleton_group, SkeletonVariance};
use revmc::OptimizationLevel;
use serde::{Deserialize, Serialize};

use crate::skelot_common::{
    collect_bytecodes, compile_per_hash, compile_skeleton_group, extract_opcode_skeleton,
    hash_bytes, per_hash_paths, skeleton_paths,
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

fn run_per_hash(
    contracts: &HashMap<B256, Vec<u8>>,
    cache_dir: &Path,
    threads: usize,
    resume: bool,
) -> (Vec<(B256, f64, u64)>, f64) {
    let opt = OptimizationLevel::Aggressive;
    let mut jobs: Vec<(B256, Vec<u8>)> = contracts.iter().map(|(h, b)| (*h, b.clone())).collect();
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
    let mut assignments: Vec<Vec<(B256, Vec<u8>)>> = (0..threads).map(|_| Vec::new()).collect();
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
                        let size = fs::metadata(&artifacts.object).map(|m| m.len()).unwrap_or(0);
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
                            let size =
                                fs::metadata(&artifacts.object).map(|m| m.len()).unwrap_or(0);
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

    // Pass 1: group by skeleton hash using only code hashes (no bytecode clones)
    let mut skeleton_groups: HashMap<u64, Vec<B256>> = HashMap::new();
    for (hash, bytes) in contracts.iter() {
        let skel = extract_opcode_skeleton(bytes);
        let skel_hash = hash_bytes(&skel);
        skeleton_groups.entry(skel_hash).or_default().push(*hash);
    }

    let mut singletons: Vec<B256> = Vec::new();
    let mut multi: Vec<(u64, Vec<B256>)> = Vec::new();
    for (skel_hash, members) in skeleton_groups {
        if members.len() >= 2 {
            multi.push((skel_hash, members));
        } else {
            singletons.push(members.into_iter().next().unwrap());
        }
    }

    eprintln!("  skeleton groups: {} families, {} singletons", multi.len(), singletons.len());

    // Pass 2: analyze variance by borrowing bytecodes from `contracts`
    let variances: Vec<(u64, Vec<B256>, SkeletonVariance)> = multi
        .into_iter()
        .map(|(skel_hash, member_hashes)| {
            let refs: Vec<&[u8]> = member_hashes.iter().map(|h| contracts[h].as_slice()).collect();
            let variance = analyze_skeleton_group(&refs);
            (skel_hash, member_hashes, variance)
        })
        .collect();

    if !resume {
        for (sh, _, _) in &variances {
            let a = skeleton_paths(cache_dir, *sh, opt);
            let _ = fs::remove_file(&a.object);
            let _ = fs::remove_file(&a.library);
        }
        for hash in &singletons {
            let a = per_hash_paths(cache_dir, hash, opt);
            let _ = fs::remove_file(&a.object);
            let _ = fs::remove_file(&a.library);
        }
    }

    let compiled = std::sync::Mutex::new(Vec::<(B256, f64, u64)>::new());
    let families_out = std::sync::Mutex::new(Vec::<FamilyRecord>::new());
    let fallback_count = AtomicUsize::new(0);
    let total_t0 = Instant::now();

    let mut assignments: Vec<Vec<(u64, Vec<B256>, SkeletonVariance)>> =
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
                for (skel_hash, member_hashes, variance) in chunk {
                    let a = skeleton_paths(cache_dir, skel_hash, opt);
                    let rep_bytes = &contracts[&member_hashes[0]];
                    let t0 = Instant::now();
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        compile_skeleton_group(
                            rep_bytes, &variance, &a.symbol, &a.object, &a.library, opt,
                        )
                    }));
                    match result {
                        Ok(Ok(())) => {
                            let dur = t0.elapsed().as_secs_f64();
                            let o_size = fs::metadata(&a.object).map(|m| m.len()).unwrap_or(0);
                            compiled_ref.lock().unwrap().push((member_hashes[0], dur, o_size));
                            families_ref.lock().unwrap().push(FamilyRecord {
                                skeleton_hash: format!("{:016x}", skel_hash),
                                members: member_hashes.len(),
                                compile_seconds: dur,
                                o_bytes: o_size,
                                fallback: false,
                            });
                        }
                        _ => {
                            eprintln!(
                                "  skel {:016x} failed, falling back to per-hash for {} members",
                                skel_hash,
                                member_hashes.len()
                            );
                            fb_ref.fetch_add(member_hashes.len(), Ordering::Relaxed);
                            for hash in &member_hashes {
                                let bytes = &contracts[hash];
                                let a = per_hash_paths(cache_dir, hash, opt);
                                let t1 = Instant::now();
                                if let Err(e) =
                                    compile_per_hash(bytes, &a.symbol, &a.object, &a.library, opt)
                                {
                                    eprintln!(
                                        "  fallback FAIL 0x{}...: {e}",
                                        &hex::encode(hash)[..12]
                                    );
                                    continue;
                                }
                                let dur = t1.elapsed().as_secs_f64();
                                let o_size = fs::metadata(&a.object).map(|m| m.len()).unwrap_or(0);
                                compiled_ref.lock().unwrap().push((*hash, dur, o_size));
                            }
                            families_ref.lock().unwrap().push(FamilyRecord {
                                skeleton_hash: format!("{:016x}", skel_hash),
                                members: member_hashes.len(),
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

    let mut singleton_assignments: Vec<Vec<B256>> = (0..threads).map(|_| Vec::new()).collect();
    for (i, hash) in singletons.into_iter().enumerate() {
        singleton_assignments[i % threads].push(hash);
    }
    std::thread::scope(|s| {
        for chunk in singleton_assignments {
            let compiled_ref = &compiled;
            s.spawn(move || {
                for hash in chunk {
                    let bytes = &contracts[&hash];
                    let a = per_hash_paths(cache_dir, &hash, opt);
                    let t0 = Instant::now();
                    if let Err(e) = compile_per_hash(bytes, &a.symbol, &a.object, &a.library, opt) {
                        eprintln!("  singleton FAIL 0x{}...: {e}", &hex::encode(hash)[..12]);
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
    (compiled.into_inner().unwrap(), families_out.into_inner().unwrap(), total_dur, fallback_total)
}

fn main() {
    let args = Args::parse();
    let contracts =
        collect_bytecodes(Path::new(&args.bench_dir), args.start_block, args.block_count);
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
    fs::write(&out_path, serde_json::to_vec_pretty(&merged).expect("serialize"))
        .expect("write compile_bench.json");
    eprintln!("wrote {}", out_path.display());
}
