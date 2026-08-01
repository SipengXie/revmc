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
use revmc::emse::{bytecode_view, group_skeleton_families, GroupingStats, SkeletonMode};
use revmc::skeleton::{
    analyze_skeleton_group_with_mode, members_conflict_static_jumps, ClassifyMode,
    SkeletonVariance,
};
use revmc::OptimizationLevel;
use serde::{Deserialize, Serialize};

use crate::skelot_common::{
    collect_bytecodes, compile_per_hash, compile_skeleton_group, per_hash_paths, skeleton_paths,
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
    #[arg(long, default_value = "current")]
    skeleton_mode: SkeletonMode,
    /// PUSH classification: "auto" (cross-instance comparison) or
    /// "force-all-variant" (ablation: every non-skip-logic PUSH via data
    /// table). Artifacts land in a separate cache subdir per combination.
    #[arg(long, default_value = "auto")]
    classify_mode: ClassifyMode,
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
struct GroupingStatsOut {
    skeleton_mode: String,
    #[serde(default)]
    classify_mode: String,
    realized_families: usize,
    per_hash_units: usize,
    g1_rejected_skeletons: usize,
    g1_rejected_members: usize,
    g2_rejected_members: usize,
    collision_groups_split: usize,
    #[serde(default)]
    static_jump_degraded_families: usize,
    #[serde(default)]
    static_jump_degraded_members: usize,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    grouping: Option<GroupingStatsOut>,
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
    sidecar: &UnitSidecar,
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
    let resumed = AtomicUsize::new(0);

    std::thread::scope(|s| {
        for chunk in assignments {
            let compiled_ref = &compiled;
            let counter_ref = &counter;
            let resumed_ref = &resumed;
            s.spawn(move || {
                for (hash, bytes) in chunk {
                    let artifacts = per_hash_paths(cache_dir, &hash, opt);
                    if resume && artifacts.library.exists() {
                        // Skipped units carry no timing: recording 0.0 ms
                        // would silently fabricate a fast arm on resume.
                        resumed_ref.fetch_add(1, Ordering::Relaxed);
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
                            sidecar.record(
                                "per_hash",
                                &format!("0x{}", hex::encode(hash)),
                                dur * 1000.0,
                                size,
                            );
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

    let resumed_total = resumed.load(Ordering::Relaxed);
    if resumed_total > 0 {
        eprintln!(
            "  WARN: resume skipped {resumed_total} pre-existing units — they are EXCLUDED from timing records; totals are not a cold-compile measurement"
        );
    }
    let total_dur = total_t0.elapsed().as_secs_f64();
    (compiled.into_inner().unwrap(), total_dur)
}

/// Per-unit JSONL sidecar (`{out}.units.jsonl`): one appended line per
/// finished compile unit, so a crash can no longer erase an arm's timing
/// detail (INCIDENT 1). One buffered write per unit (<100 B) against
/// 100ms-scale unit compiles; symmetric across arms.
struct UnitSidecar(Option<std::sync::Mutex<fs::File>>);

impl UnitSidecar {
    fn open(out: &str) -> Self {
        let path = format!("{out}.units.jsonl");
        if let Some(parent) = Path::new(&path).parent() {
            fs::create_dir_all(parent).ok();
        }
        match fs::OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => Self(Some(std::sync::Mutex::new(f))),
            Err(e) => {
                eprintln!("  WARN: cannot open unit sidecar {path}: {e}; detail persists only in the final JSON");
                Self(None)
            }
        }
    }

    fn record(&self, mode: &str, unit: &str, ms: f64, o_bytes: u64) {
        if let Some(f) = &self.0 {
            use std::io::Write;
            let line = format!("{{\"mode\":\"{mode}\",\"unit\":\"{unit}\",\"ms\":{ms:.3},\"o_bytes\":{o_bytes}}}\n");
            if let Ok(mut g) = f.lock() {
                let _ = g.write_all(line.as_bytes());
            }
        }
    }
}

/// Persist one arm's record immediately (arm-level durability): merge into
/// the existing output file, refusing duplicate (scope, mode) pairs so a
/// rerun cannot silently stack stale records for downstream consumers.
fn append_record_now(out: &str, record: CompileRecord) {
    let out_path = PathBuf::from(out);
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent).ok();
    }
    let mut all: CompileBenchOut = if out_path.exists() {
        serde_json::from_slice(&fs::read(&out_path).expect("read out"))
            .expect("parse existing out")
    } else {
        CompileBenchOut { records: Vec::new() }
    };
    if all.records.iter().any(|r| r.scope == record.scope && r.mode == record.mode) {
        panic!(
            "{out} already holds a record for scope={} mode={}; refusing to append a duplicate — use a fresh --out or remove the stale record",
            record.scope, record.mode
        );
    }
    all.records.push(record);
    fs::write(&out_path, serde_json::to_vec_pretty(&all).expect("serialize"))
        .expect("write compile_bench.json");
    eprintln!("  wrote {} ({} records)", out_path.display(), all.records.len());
}

struct SkeletonRunOutcome {
    items: Vec<(B256, f64, u64)>,
    families: Vec<FamilyRecord>,
    total_seconds: f64,
    fallback_count: usize,
    stats: GroupingStats,
    realized_families: usize,
    per_hash_units: usize,
    static_jump_degraded_families: usize,
    static_jump_degraded_members: usize,
}

fn run_skeleton(
    contracts: &HashMap<B256, Vec<u8>>,
    cache_dir: &Path,
    threads: usize,
    resume: bool,
    skeleton_mode: SkeletonMode,
    classify_mode: ClassifyMode,
    sidecar: &UnitSidecar,
) -> SkeletonRunOutcome {
    let opt = OptimizationLevel::Aggressive;

    // Grouping with in-bucket skeleton-byte verification; under EMSE the
    // admission guards run here and rejected members land in `per_hash`.
    let grouping = group_skeleton_families(contracts, skeleton_mode);
    let mut singletons = grouping.per_hash;
    let stats = grouping.stats;

    eprintln!(
        "  skeleton groups ({}): {} families, {} per-hash units",
        skeleton_mode.as_str(),
        grouping.families.len(),
        singletons.len()
    );
    eprintln!(
        "  admission: g1 rejected {} skeletons ({} members), g2 rejected {} members, {} collision groups split",
        stats.g1_rejected_skeletons,
        stats.g1_rejected_members,
        stats.g2_rejected_members,
        stats.collision_groups_split
    );

    // Analyze variance over the mode's bytecode view (EMSE: stripped).
    // Families whose members jump to different static targets cannot share
    // one artifact (the backend folds those jumps at compile time) — degrade
    // them to per-hash up front instead of panicking in apply_variance.
    let mut static_jump_degraded_families = 0usize;
    let mut static_jump_degraded_members = 0usize;
    let variances: Vec<(u64, Vec<B256>, SkeletonVariance)> = grouping
        .families
        .into_iter()
        .filter_map(|family| {
            let views: Vec<&[u8]> = family
                .members
                .iter()
                .map(|h| bytecode_view(&contracts[h], skeleton_mode))
                .collect();
            let variance = analyze_skeleton_group_with_mode(&views, classify_mode);
            if members_conflict_static_jumps(&views) {
                eprintln!(
                    "  skel {:016x}: {} members degraded to per-hash (variant PUSH feeds a static jump)",
                    family.skel_hash,
                    family.members.len()
                );
                static_jump_degraded_families += 1;
                static_jump_degraded_members += family.members.len();
                singletons.extend(family.members);
                return None;
            }
            Some((family.skel_hash, family.members, variance))
        })
        .collect();
    let per_hash_units = singletons.len();
    let realized_families = variances.len();
    if static_jump_degraded_families > 0 {
        eprintln!(
            "  static-jump degradations: {} families ({} members) -> per-hash",
            static_jump_degraded_families, static_jump_degraded_members
        );
    }

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
    let fam_done = AtomicUsize::new(0);
    let resumed = AtomicUsize::new(0);
    let total_t0 = Instant::now();

    // Longest-processing-time-first, matching the per-hash arm: dealing in
    // arbitrary order leaves a long tail of idle threads and depresses the
    // wall-clock ratio for scheduling reasons rather than compilation ones.
    let mut variances = variances;
    variances.sort_by_key(|(_, members, _)| {
        std::cmp::Reverse(contracts.get(&members[0]).map_or(0, |b| b.len()))
    });
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
            let fam_done_ref = &fam_done;
            let resumed_ref = &resumed;
            s.spawn(move || {
                for (skel_hash, member_hashes, variance) in chunk {
                    let a = skeleton_paths(cache_dir, skel_hash, opt);
                    if resume && a.library.exists() {
                        // Same rule as the per-hash arm: a skipped unit carries
                        // no timing, otherwise a resumed SkelOT arm would be
                        // compared against a cold per-hash arm.
                        resumed_ref.fetch_add(1, Ordering::Relaxed);
                        fam_done_ref.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    let rep_bytes = bytecode_view(&contracts[&member_hashes[0]], skeleton_mode);
                    let t0 = Instant::now();
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        compile_skeleton_group(
                            rep_bytes, &variance, &a.symbol, &a.object, &a.library, opt,
                        )
                    }));
                    let done = fam_done_ref.fetch_add(1, Ordering::Relaxed) + 1;
                    if done % 50 == 0 {
                        eprintln!(
                            "  [skelot family {done}] {:.1}s",
                            total_t0.elapsed().as_secs_f64()
                        );
                    }
                    match result {
                        Ok(Ok(())) => {
                            let dur = t0.elapsed().as_secs_f64();
                            let o_size = fs::metadata(&a.object).map(|m| m.len()).unwrap_or(0);
                            compiled_ref.lock().unwrap().push((member_hashes[0], dur, o_size));
                            sidecar.record(
                                "skelot",
                                &format!("skel_{skel_hash:016x}"),
                                dur * 1000.0,
                                o_size,
                            );
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
                                sidecar.record(
                                    "skelot",
                                    &format!("0x{}", hex::encode(hash)),
                                    dur * 1000.0,
                                    o_size,
                                );
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

    let mut singletons = singletons;
    singletons.sort_by_key(|h| std::cmp::Reverse(contracts.get(h).map_or(0, |b| b.len())));
    let mut singleton_assignments: Vec<Vec<B256>> = (0..threads).map(|_| Vec::new()).collect();
    for (i, hash) in singletons.into_iter().enumerate() {
        singleton_assignments[i % threads].push(hash);
    }
    let single_done = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for chunk in singleton_assignments {
            let compiled_ref = &compiled;
            let single_done_ref = &single_done;
            let resumed_ref = &resumed;
            s.spawn(move || {
                for hash in chunk {
                    let bytes = &contracts[&hash];
                    let a = per_hash_paths(cache_dir, &hash, opt);
                    if resume && a.library.exists() {
                        resumed_ref.fetch_add(1, Ordering::Relaxed);
                        single_done_ref.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    let t0 = Instant::now();
                    if let Err(e) = compile_per_hash(bytes, &a.symbol, &a.object, &a.library, opt) {
                        eprintln!("  singleton FAIL 0x{}...: {e}", &hex::encode(hash)[..12]);
                        continue;
                    }
                    let dur = t0.elapsed().as_secs_f64();
                    let o_size = fs::metadata(&a.object).map(|m| m.len()).unwrap_or(0);
                    compiled_ref.lock().unwrap().push((hash, dur, o_size));
                    sidecar.record("skelot", &format!("0x{}", hex::encode(hash)), dur * 1000.0, o_size);
                    let done = single_done_ref.fetch_add(1, Ordering::Relaxed) + 1;
                    if done % 500 == 0 {
                        eprintln!(
                            "  [skelot singleton {done}] {:.1}s",
                            total_t0.elapsed().as_secs_f64()
                        );
                    }
                }
            });
        }
    });

    let resumed_total = resumed.load(Ordering::Relaxed);
    if resumed_total > 0 {
        eprintln!(
            "  WARN: resume skipped {resumed_total} pre-existing units — they are EXCLUDED from timing records; totals are not a cold-compile measurement"
        );
    }
    let total_dur = total_t0.elapsed().as_secs_f64();
    let fallback_total = fallback_count.load(Ordering::Relaxed);
    SkeletonRunOutcome {
        items: compiled.into_inner().unwrap(),
        families: families_out.into_inner().unwrap(),
        total_seconds: total_dur,
        fallback_count: fallback_total,
        stats,
        realized_families,
        per_hash_units,
        static_jump_degraded_families,
        static_jump_degraded_members,
    }
}

fn main() {
    let args = Args::parse();
    let classify_mode = args.classify_mode;
    let contracts =
        collect_bytecodes(Path::new(&args.bench_dir), args.start_block, args.block_count);
    eprintln!("Collected {} compilable bytecodes", contracts.len());
    let cache_dir = PathBuf::from(&args.cache_dir);
    fs::create_dir_all(&cache_dir).expect("mk cache");
    let sidecar = UnitSidecar::open(&args.out);

    if args.mode == "per-hash" || args.mode == "both" {
        let ph_cache = cache_dir.join("per_hash");
        fs::create_dir_all(&ph_cache).ok();
        let (items, total) =
            run_per_hash(&contracts, &ph_cache, args.threads, args.resume, &sidecar);
        let total_o: u64 = items.iter().map(|(_, _, s)| *s).sum();
        append_record_now(&args.out, CompileRecord {
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
            grouping: None,
        });
    }
    if args.mode == "skeleton" || args.mode == "both" {
        let sk_cache = cache_dir.join(match (args.skeleton_mode, classify_mode) {
            (SkeletonMode::Current, ClassifyMode::Auto) => "skelot",
            (SkeletonMode::Emse, ClassifyMode::Auto) => "skelot_emse",
            (SkeletonMode::Current, ClassifyMode::ForceAllVariant) => "skelot_forced",
            (SkeletonMode::Emse, ClassifyMode::ForceAllVariant) => "skelot_emse_forced",
        });
        fs::create_dir_all(&sk_cache).ok();
        let run = run_skeleton(
            &contracts,
            &sk_cache,
            args.threads,
            args.resume,
            args.skeleton_mode,
            classify_mode,
            &sidecar,
        );
        let total_o: u64 = run.items.iter().map(|(_, _, s)| *s).sum();
        append_record_now(&args.out, CompileRecord {
            scope: args.scope.clone(),
            mode: "skelot".into(),
            n_units: run.items.len(),
            total_compile_seconds: run.total_seconds,
            total_o_bytes: total_o,
            per_unit_ms: run.items.iter().map(|(_, d, _)| d * 1000.0).collect(),
            per_unit_o_bytes: run.items.iter().map(|(_, _, s)| *s).collect(),
            compile_unit_hashes: run
                .items
                .iter()
                .map(|(h, _, _)| format!("0x{}", hex::encode(h)))
                .collect(),
            per_family: Some(run.families),
            fallback_count: run.fallback_count,
            grouping: Some(GroupingStatsOut {
                skeleton_mode: args.skeleton_mode.as_str().to_string(),
                classify_mode: classify_mode.as_str().to_string(),
                realized_families: run.realized_families,
                per_hash_units: run.per_hash_units,
                g1_rejected_skeletons: run.stats.g1_rejected_skeletons,
                g1_rejected_members: run.stats.g1_rejected_members,
                g2_rejected_members: run.stats.g2_rejected_members,
                collision_groups_split: run.stats.collision_groups_split,
                static_jump_degraded_families: run.static_jump_degraded_families,
                static_jump_degraded_members: run.static_jump_degraded_members,
            }),
        });
    }
}
