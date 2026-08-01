#[path = "../bin_common.rs"]
#[allow(dead_code)]
mod bin_common;

#[path = "../skelot_common.rs"]
#[allow(dead_code)]
mod skelot_common;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::Parser;
use revm::primitives::B256;
use revmc::skeleton::{
    analyze_skeleton_group_with_mode, ClassifyMode, SkeletonVariance,
};
use revmc::OptimizationLevel;
use revmc_context::RawEvmCompilerFn;
use serde::{Deserialize, Serialize};

use bin_common::{
    build_tx_index_cached, BinLoader, TxBin,
};
use skelot_common::{
    collect_bytecodes_cached, compile_per_hash, compile_skeleton_group, stratified_family_sample,
    FamilySummary,
};

const OPT: OptimizationLevel = OptimizationLevel::Aggressive;

#[derive(Parser, Debug)]
#[command(about = "Stage E — All-Variant SkelOT ablation benchmark")]
struct Args {
    /// Path to Stage A's skeleton_families.json.
    #[arg(long)]
    families: PathBuf,
    /// Bench data root (containing states/ and txs/).
    #[arg(long)]
    bench_dir: PathBuf,
    /// Families per bucket (final: 20; smoke: 1).
    #[arg(long, default_value_t = 20)]
    per_bucket: usize,
    /// Exec rounds (final: 1000; smoke: 50).
    #[arg(long, default_value_t = 1000)]
    rounds: usize,
    /// Exec warmup rounds.
    #[arg(long, default_value_t = 50)]
    warmup: usize,
    /// Welch alpha (reserved, recorded in meta).
    #[arg(long, default_value_t = 0.05)]
    alpha: f64,
    /// Deterministic seed (reserved).
    #[arg(long, default_value_t = 42)]
    seed: u64,
    /// First block (inclusive) whose `states/{bn}.bin` feeds the bytecode corpus.
    #[arg(long, default_value_t = 38004930)]
    start_block: u64,
    /// Number of blocks to scan from `start_block`.
    #[arg(long, default_value_t = 10000)]
    block_count: u64,
    /// Max resolved bytecodes per family for the compile phase and dispatch
    /// target set. per_hash compile cost scales linearly with this; SkelOT /
    /// all_variant compile once per family regardless. Truncation is
    /// deterministic (sorted by hash lex).
    #[arg(long, default_value_t = 128)]
    compile_members_cap: usize,
    /// Output JSON path.
    #[arg(long)]
    out: PathBuf,
    /// Resume from the per-family JSONL sidecar next to `--out`. When `true`
    /// (default), already-completed families skip compile+exec and are
    /// reloaded from the sidecar. Pass `--no-resume` to discard the sidecar
    /// and recompute everything.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    resume: bool,
    /// Worker threads for per_hash compile within a family. `compile_time_ms`
    /// still reports the sum of per-unit compile wall (serial equivalent) so
    /// cap-invariant comparisons across modes and families are preserved;
    /// parallelism only reduces overall Task wall-clock. Skeleton and
    /// all-variant modes are a single compile per family and ignore this.
    #[arg(long, default_value_t = 8)]
    compile_threads: usize,
    /// Representative-transaction index built from executed FRAMES
    /// (`frame_tx_index`) instead of the default `tx.to` scan. The default
    /// index only sees contracts a transaction addresses directly, so
    /// families reached solely through internal calls get no representative
    /// and are skipped in the exec phase.
    #[arg(long)]
    tx_index: Option<PathBuf>,
}

/// Sidecar JSONL path derived from the final output path.
/// `/…/foo.json` → `/…/foo.families.jsonl`.
fn sidecar_path(out: &Path) -> PathBuf {
    let stem = out.file_stem().and_then(|s| s.to_str()).unwrap_or("ablation");
    let parent = out.parent().unwrap_or(Path::new("."));
    parent.join(format!("{stem}.families.jsonl"))
}

/// Read every JSON line from `path` and collect the `skeleton_id` field.
fn load_sidecar_done(path: &Path) -> std::io::Result<(HashSet<String>, Vec<serde_json::Value>)> {
    use std::io::{BufRead, BufReader};
    let mut done = HashSet::new();
    let mut rows = Vec::new();
    let f = std::fs::File::open(path)?;
    for line in BufReader::new(f).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[resume] skipping malformed sidecar line: {e}");
                continue;
            }
        };
        if let Some(id) = v.get("skeleton_id").and_then(|s| s.as_str()) {
            done.insert(id.to_string());
            rows.push(v);
        }
    }
    Ok((done, rows))
}

/// Append one family row as a single JSON line + newline. Creates the file
/// if missing; flushes before returning.
fn append_family_row(path: &Path, row: &serde_json::Value) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    let line = serde_json::to_string(row)?;
    f.write_all(line.as_bytes())?;
    f.write_all(b"\n")?;
    f.flush()?;
    Ok(())
}

#[derive(Debug, Deserialize)]
struct StageAFamily {
    skeleton_hash: String,
    skeleton_len: usize,
    family_size: usize,
    bucket: String,
}

#[derive(Debug, Deserialize)]
struct StageAFamiliesFile {
    families: Vec<StageAFamily>,
}

fn load_stage_a_families(families_path: &std::path::Path) -> anyhow::Result<Vec<FamilySummary>> {
    let file: StageAFamiliesFile = serde_json::from_reader(std::fs::File::open(families_path)?)?;
    let tsv_path = families_path.with_file_name("skeleton_mapping.tsv");
    let tsv = std::fs::read_to_string(&tsv_path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", tsv_path.display()))?;
    let mut by_skel: HashMap<String, Vec<String>> = HashMap::new();
    for line in tsv.lines() {
        let mut it = line.split('\t');
        if let (Some(s), Some(h)) = (it.next(), it.next()) {
            by_skel.entry(s.to_string()).or_default().push(h.to_string());
        }
    }
    Ok(file
        .families
        .into_iter()
        .map(|f| {
            let member_hashes = by_skel.remove(&f.skeleton_hash).unwrap_or_else(|| {
                eprintln!("[warn] skeleton {} has 0 mapped hashes", f.skeleton_hash);
                Vec::new()
            });
            FamilySummary {
                skeleton_id: f.skeleton_hash,
                skeleton_len: f.skeleton_len,
                bucket: f.bucket,
                member_count: f.family_size,
                member_hashes,
            }
        })
        .collect())
}

#[derive(Debug, Serialize)]
struct CompileMetrics {
    compile_time_ms: f64,
    so_bytes: u64,
    num_variant: u32,
    num_invariant: u32,
    /// PUSHes kept Invariant specifically because their immediate is consumed
    /// by a static JUMP/JUMPI. Meaningful only for ForceAllVariant; 0 for
    /// per_hash and Auto modes.
    num_skip_logic_preserved: u32,
}

fn count_push_immediates(bytecode: &[u8]) -> usize {
    let mut n = 0usize;
    let mut i = 0;
    while i < bytecode.len() {
        let op = bytecode[i];
        i += 1;
        if (0x60..=0x7f).contains(&op) {
            n += 1;
            let len = (op - 0x5f) as usize;
            i = (i + len).min(bytecode.len());
        }
    }
    n
}

fn resolve_family_members(
    all_codes: &HashMap<B256, Vec<u8>>,
    fam: &FamilySummary,
) -> Vec<(String, B256, Vec<u8>)> {
    use std::str::FromStr;
    fam.member_hashes
        .iter()
        .filter_map(|h| {
            let key = B256::from_str(h.trim_start_matches("0x")).ok()?;
            let code = all_codes.get(&key)?;
            Some((h.clone(), key, code.clone()))
        })
        .collect()
}

/// Cap deterministically: members with txs in corpus first (sorted by
/// (earliest block, tx_index)), then fill remainder by hash-sort. Guarantees
/// at least one capped member has a tx if the family has any observations.
/// Returns the capped member set plus ALL candidate representative
/// transactions (sorted, deduped). A single candidate is not enough: the
/// exec phase rebuilds pre-tx state by replaying the block prefix, which can
/// fail on that particular block (e.g. an OP deposit whose mint the uniform
/// EVM handler does not apply). The caller walks the list until one works.
fn cap_members(
    resolved: Vec<(String, B256, Vec<u8>)>,
    tx_index: &HashMap<B256, (u64, usize)>,
    cap: usize,
) -> (Vec<(B256, Vec<u8>)>, Vec<(u64, usize)>) {
    let mut with_tx: Vec<(u64, usize, String, B256, Vec<u8>)> = Vec::new();
    let mut without_tx: Vec<(String, B256, Vec<u8>)> = Vec::new();
    for (hstr, key, code) in resolved {
        match tx_index.get(&key) {
            Some(&(bn, ti)) => with_tx.push((bn, ti, hstr, key, code)),
            None => without_tx.push((hstr, key, code)),
        }
    }
    with_tx.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    without_tx.sort_by(|a, b| a.0.cmp(&b.0));
    let mut rep_tx: Vec<(u64, usize)> = with_tx.iter().map(|(bn, ti, _, _, _)| (*bn, *ti)).collect();
    rep_tx.dedup();
    rep_tx.truncate(8);
    let mut out: Vec<(B256, Vec<u8>)> = with_tx.into_iter().map(|(_, _, _, h, v)| (h, v)).collect();
    if out.len() < cap {
        let need = cap - out.len();
        out.extend(without_tx.into_iter().take(need).map(|(_, h, v)| (h, v)));
    } else {
        out.truncate(cap);
    }
    (out, rep_tx)
}

/// Per-hash artifact map keyed by code hash.
struct PerHashArtifact {
    hash: B256,
    lib: PathBuf,
    symbol: String,
}

/// Persistent `.so` cache root for Stage E. Files survive across runs; keyed
/// by cap so different `--compile-members-cap` settings don't collide.
const SO_CACHE_ROOT: &str =
    "/home/ubuntu/hermes-shared-workspace/skelot-paper/data/stage_e_so_cache";

fn family_cache_dir(skeleton_id: &str, cap: usize) -> PathBuf {
    Path::new(SO_CACHE_ROOT).join(format!("cap{cap}")).join(skeleton_id)
}

fn cache_hit(obj: &Path, lib: &Path) -> bool {
    let obj_ok = std::fs::metadata(obj).map(|m| m.len() > 0).unwrap_or(false);
    let lib_ok = std::fs::metadata(lib).map(|m| m.len() > 0).unwrap_or(false);
    obj_ok && lib_ok
}

fn compile_per_hash_mode(
    members: &[(B256, Vec<u8>)],
    cache_dir: &Path,
    threads: usize,
) -> anyhow::Result<(CompileMetrics, Vec<PerHashArtifact>)> {
    use std::sync::Mutex;
    // Round-robin assign compiles to N worker threads. Each thread maintains
    // its own LLVM Context (via compile_per_hash's per-call scoping), so the
    // pattern is identical to Stage B's `run_per_hash` in skelot_compile_bench.
    //
    // Cache-hit semantics: if a family member's .o + .so are already on disk
    // under `{cache_dir}/per_hash_{hash16}.*` with non-zero size, skip compile
    // entirely, record `compile_time_ms = 0.0` for that member, and report the
    // existing `.so` size. Cache misses compile + write.
    //
    // `compile_time_ms` aggregates per-unit wall (serial-equivalent) NOT the
    // parallel wall, keeping cap-invariant ratios meaningful across families.
    std::fs::create_dir_all(cache_dir)?;
    let n = members.len();
    let threads = threads.max(1).min(n.max(1));
    let mut assignments: Vec<Vec<usize>> = (0..threads).map(|_| Vec::new()).collect();
    for i in 0..n {
        assignments[i % threads].push(i);
    }

    type Slot = Mutex<Option<(PerHashArtifact, f64, u64)>>;
    let slots: Vec<Slot> = (0..n).map(|_| Mutex::new(None)).collect();
    let slots_ref = &slots;
    let members_ref = members;

    std::thread::scope(|s| -> anyhow::Result<()> {
        let handles: Vec<_> = assignments
            .into_iter()
            .map(|chunk| {
                s.spawn(move || -> anyhow::Result<()> {
                    for i in chunk {
                        let (hash, bc) = &members_ref[i];
                        let hash_hex = hex::encode(&hash[..8]); // 16 hex = 64-bit
                        let symbol = format!("ph_{hash_hex}");
                        let obj = cache_dir.join(format!("per_hash_{hash_hex}.o"));
                        let lib = cache_dir.join(format!("per_hash_{hash_hex}.so"));
                        let (unit_ms, size) = if cache_hit(&obj, &lib) {
                            (0.0_f64, std::fs::metadata(&lib)?.len())
                        } else {
                            let t0 = Instant::now();
                            compile_per_hash(bc, &symbol, &obj, &lib, OPT)
                                .map_err(|e| anyhow::anyhow!("compile_per_hash[{i}]: {e}"))?;
                            let ms = t0.elapsed().as_secs_f64() * 1000.0;
                            (ms, std::fs::metadata(&lib)?.len())
                        };
                        *slots_ref[i].lock().unwrap() =
                            Some((PerHashArtifact { hash: *hash, lib, symbol }, unit_ms, size));
                    }
                    Ok(())
                })
            })
            .collect();
        for h in handles {
            h.join().map_err(|e| anyhow::anyhow!("per_hash worker panic: {e:?}"))??;
        }
        Ok(())
    })?;

    let mut arts = Vec::with_capacity(n);
    let mut compile_time_ms = 0.0f64;
    let mut so_bytes = 0u64;
    for slot in slots {
        let (art, unit_ms, size) =
            slot.into_inner().unwrap().expect("every per_hash unit must be filled");
        arts.push(art);
        compile_time_ms += unit_ms;
        so_bytes += size;
    }

    let total_push = members.first().map(|(_, bc)| count_push_immediates(bc) as u32).unwrap_or(0);
    Ok((
        CompileMetrics {
            compile_time_ms,
            so_bytes,
            num_variant: 0,
            num_invariant: total_push,
            num_skip_logic_preserved: 0,
        },
        arts,
    ))
}

struct SkeletonArtifact {
    lib: PathBuf,
    symbol: String,
    variance: SkeletonVariance,
}

fn compile_skeleton_mode(
    members: &[(B256, Vec<u8>)],
    mode: ClassifyMode,
    cache_dir: &Path,
    tag: &str,
) -> anyhow::Result<(CompileMetrics, SkeletonArtifact)> {
    std::fs::create_dir_all(cache_dir)?;
    let refs: Vec<&[u8]> = members.iter().map(|(_, v)| v.as_slice()).collect();
    let variance = analyze_skeleton_group_with_mode(&refs, mode);
    let obj = cache_dir.join(format!("{tag}.o"));
    let lib = cache_dir.join(format!("{tag}.so"));
    let (compile_time_ms, so_bytes) = if cache_hit(&obj, &lib) {
        (0.0_f64, std::fs::metadata(&lib)?.len())
    } else {
        let t = Instant::now();
        compile_skeleton_group(&members[0].1, &variance, tag, &obj, &lib, OPT)
            .map_err(|e| anyhow::anyhow!("compile_skeleton_group: {e}"))?;
        (t.elapsed().as_secs_f64() * 1000.0, std::fs::metadata(&lib)?.len())
    };
    let num_total = variance.pushes.len() as u32;
    let num_invariant = num_total - variance.num_variant;
    let num_skip_logic_preserved = match mode {
        ClassifyMode::ForceAllVariant => num_invariant,
        ClassifyMode::Auto => 0,
    };
    Ok((
        CompileMetrics {
            compile_time_ms,
            so_bytes,
            num_variant: variance.num_variant,
            num_invariant,
            num_skip_logic_preserved,
        },
        SkeletonArtifact { lib, symbol: tag.to_string(), variance },
    ))
}

#[path = "../ablation_exec.rs"]
mod ablation_exec;
use ablation_exec::{flat_mode, geomean, run_family_exec};

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let mut families = load_stage_a_families(&args.families)?;

    // Drop families no member of which ever executes. The corpus is every
    // contract the node had to LOAD, which includes code referenced only by
    // EXTCODESIZE ("is this a contract?" guards) — 2,821 of 19,130 on Base
    // 10K, 100% EXTCODESIZE-referenced. Such families can never yield a
    // runtime measurement, so sampling them wastes a stratum slot and
    // leaves the runtime column structurally empty. The frame-level tx
    // index's key set IS the executed set, so `--tx-index` doubles as the
    // eligibility filter.
    if let Some(p) = &args.tx_index {
        let data = std::fs::read(p).expect("read --tx-index");
        let idx: HashMap<B256, (u64, usize)> =
            bincode::deserialize(&data).expect("deserialize --tx-index");
        let before = families.len();
        families.retain(|f| {
            f.member_hashes.iter().any(|h| {
                <B256 as std::str::FromStr>::from_str(h.trim_start_matches("0x"))
                    .map(|k| idx.contains_key(&k))
                    .unwrap_or(false)
            })
        });
        eprintln!(
            "[eligibility] {} of {before} families have >=1 executed member; {} dropped (load-only)",
            families.len(),
            before - families.len()
        );
    }

    let sampled = stratified_family_sample(&families, args.per_bucket, args.seed);
    eprintln!("Sampled {} families from {} total", sampled.len(), families.len());

    // Resumable persistence: each completed family is appended as one JSON
    // line to `{out-stem}.families.jsonl` next to --out. On restart we read
    // the sidecar, skip already-done skeleton_ids, and at end reload all
    // rows from it to build bucket_aggregates and the final JSON.
    let sidecar = sidecar_path(&args.out);
    if !args.resume {
        let _ = std::fs::remove_file(&sidecar);
        eprintln!("[resume] --no-resume: cleared sidecar {}", sidecar.display());
    }
    let (mut done_ids, prior_rows) = load_sidecar_done(&sidecar).unwrap_or_default();
    // v1 → v2 migration: rows lacking `frames_per_round_mean` were written under
    // the per-tx-sum exec semantic. They are not combinable with per-frame
    // samples, so we move the old sidecar aside (preserving provenance) and
    // recompute every sampled family once under v2 semantics. After the first
    // post-refactor run, the sidecar is pure v2 and resumes normally.
    let has_v1 = prior_rows
        .iter()
        .any(|r| r["per_hash"].get("frames_per_round_mean").map_or(true, |v| v.is_null()));
    if has_v1 && !prior_rows.is_empty() {
        let v1_backup = sidecar.with_extension("v1.jsonl");
        std::fs::rename(&sidecar, &v1_backup)?;
        eprintln!(
            "[migrate] v1 sidecar → {}; recomputing {} families under per-frame semantics",
            v1_backup.display(),
            prior_rows.len()
        );
        done_ids.clear();
    }
    if !done_ids.is_empty() {
        eprintln!(
            "[resume] loaded {} prior family rows from {}",
            prior_rows.len(),
            sidecar.display()
        );
    }
    let all_done = sampled.iter().all(|f| done_ids.contains(&f.skeleton_id));

    // Skip the corpus scan + tx-index build entirely when the sidecar already
    // covers every sampled family. Otherwise, load the cached indices so
    // subsequent runs reuse the scan result instead of paying ~8 min per
    // restart. Each entry is `(capped_members, earliest_rep_block)`.
    let family_plans: Vec<(Vec<(B256, Vec<u8>)>, Vec<(u64, usize)>)> = if all_done {
        eprintln!(
            "[resume] all {} sampled families already in sidecar; skipping corpus scan",
            sampled.len()
        );
        vec![(Vec::new(), Vec::new()); sampled.len()]
    } else {
        let all_codes =
            collect_bytecodes_cached(&args.bench_dir, args.start_block, args.block_count);
        eprintln!(
            "Collected {} compilable bytecodes from states/[{}..{})",
            all_codes.len(),
            args.start_block,
            args.start_block + args.block_count
        );

        // Two-phase cap: resolve ALL family members, build tx-index over them,
        // then cap preferring members with txs in corpus. Guarantees the
        // capped set has a rep whenever the family has any observed txs.
        let resolved_per_family: Vec<Vec<(String, B256, Vec<u8>)>> =
            sampled.iter().map(|fam| resolve_family_members(&all_codes, fam)).collect();
        let tx_index = match &args.tx_index {
            Some(p) => {
                let data = std::fs::read(p).expect("read --tx-index");
                let map: HashMap<B256, (u64, usize)> =
                    bincode::deserialize(&data).expect("deserialize --tx-index");
                eprintln!("[tx-index] loaded {} frame-level entries from {}", map.len(), p.display());
                map
            }
            None => build_tx_index_cached(&args.bench_dir, args.start_block, args.block_count),
        };

        resolved_per_family
            .into_iter()
            .map(|r| cap_members(r, &tx_index, args.compile_members_cap))
            .collect()
    };

    for (idx, (fam, (members, rep_tx_from_cap))) in
        sampled.iter().zip(family_plans.iter()).enumerate()
    {
        if done_ids.contains(&fam.skeleton_id) {
            eprintln!(
                "[{}/{}] [resume] skipping already-done family {}",
                idx + 1,
                sampled.len(),
                fam.skeleton_id
            );
            continue;
        }
        if members.is_empty() {
            eprintln!("[skip] family {}: no bytecodes resolved from corpus", fam.skeleton_id);
            continue;
        }
        {
            let refs: Vec<&[u8]> = members.iter().map(|(_, v)| v.as_slice()).collect();
            // This driver analyses RAW bytecodes, so a family file produced by
            // `skelot_dataset_scan --skeleton-mode emse` (members share a
            // skeleton only AFTER metadata stripping) has members with
            // different raw PUSH counts. Feeding those to
            // analyze_skeleton_group_with_mode trips its assert, and under
            // `panic = "abort"` that kills the whole multi-hour run instead of
            // skipping one family.
            let ref_pushes = revmc::skeleton::extract_push_immediates(refs[0]).len();
            if refs[1..]
                .iter()
                .any(|b| revmc::skeleton::extract_push_immediates(b).len() != ref_pushes)
            {
                eprintln!(
                    "[skip] family {}: members disagree on raw PUSH count — this family file was \
                     built under a different skeleton boundary (EMSE?) than this driver analyses",
                    fam.skeleton_id
                );
                continue;
            }
            // Static-jump-conflicting families are degraded to per-hash by the
            // compile drivers and never skeleton-dispatched; skip them here.
            if revmc::skeleton::members_conflict_static_jumps(&refs) {
                eprintln!(
                    "[skip] family {}: variant PUSH feeds a static jump (degraded family)",
                    fam.skeleton_id
                );
                continue;
            }
        }
        // Persistent `.so` cache dir — survives across runs. Cache keyed by
        // cap to auto-invalidate when `--compile-members-cap` changes.
        let fam_cache = family_cache_dir(&fam.skeleton_id, args.compile_members_cap);

        // ── Compile (3 modes, cache-aware) ────────────────────────────────
        let (per_hash, ph_arts) = compile_per_hash_mode(members, &fam_cache, args.compile_threads)?;
        let (skelot, sk_art) =
            compile_skeleton_mode(members, ClassifyMode::Auto, &fam_cache, "skelot_auto")?;
        let (all_variant, av_art) = compile_skeleton_mode(
            members,
            ClassifyMode::ForceAllVariant,
            &fam_cache,
            "skelot_forced",
        )?;

        let bucket_name = fam.bucket.as_str();

        // Rep (block, tx_index) is computed during cap_members so we don't
        // need tx_index in this scope (the all-done fast path doesn't build
        // it). Exec replays THIS single tx per round on the pre-tx snapshot.
        let rep_tx: &[(u64, usize)] = rep_tx_from_cap.as_slice();

        let (ph_exec, sk_exec, av_exec) = run_family_exec(
            &fam.skeleton_id,
            members,
            rep_tx,
            &args.bench_dir,
            &ph_arts,
            &sk_art,
            &av_art,
            args.rounds,
            args.warmup,
        );

        // ── Flatten ───────────────────────────────────────────────────────
        let ph_row = flat_mode(&per_hash, &ph_exec, Some(&sk_exec));
        let sk_row = flat_mode(&skelot, &sk_exec, None);
        let av_row = flat_mode(&all_variant, &av_exec, Some(&sk_exec));

        let row = serde_json::json!({
            "skeleton_id": fam.skeleton_id,
            "bucket": bucket_name,
            "member_count": fam.member_count,
            "members_compiled": members.len(),
            "rep_block": rep_tx.first().map(|(bn, _)| *bn),
            "rep_tx_index": rep_tx.first().map(|(_, ti)| *ti),
            "rep_candidates": rep_tx.len(),
            "per_hash": ph_row,
            "skelot": sk_row,
            "all_variant": av_row,
        });
        append_family_row(&sidecar, &row)?;
        eprintln!(
            "[{}/{}] {} done (rep_tx={:?}, frames=[ph={} av_mean={:.2}, sk={} av_mean={:.2}, av={} av_mean={:.2}])",
            idx + 1,
            sampled.len(),
            fam.skeleton_id,
            rep_tx,
            ph_exec.frame_samples_n,
            ph_exec.frames_per_round_mean,
            sk_exec.frame_samples_n,
            sk_exec.frames_per_round_mean,
            av_exec.frame_samples_n,
            av_exec.frames_per_round_mean,
        );
    }

    // Reload the authoritative family rows from the sidecar. On a plain run
    // this is a superset of what we just appended; on resume it's the union
    // of prior + new rows. Source of truth for bucket_aggregates and the
    // final JSON.
    let (_final_ids, fam_rows) = load_sidecar_done(&sidecar).unwrap_or_default();
    eprintln!("Assembling final output from {} sidecar rows", fam_rows.len());

    // ── Bucket aggregates ─────────────────────────────────────────────────
    let mut by_bucket: std::collections::BTreeMap<String, Vec<&serde_json::Value>> =
        Default::default();
    for row in &fam_rows {
        let b = row["bucket"].as_str().unwrap_or("?").to_string();
        by_bucket.entry(b).or_default().push(row);
    }
    let mut aggregates = Vec::new();
    for (bucket, rows) in by_bucket {
        let ct: Vec<f64> = rows
            .iter()
            .map(|r| {
                r["all_variant"]["compile_time_ms"].as_f64().unwrap_or(0.0)
                    / r["skelot"]["compile_time_ms"].as_f64().unwrap_or(1e-9).max(1e-9)
            })
            .collect();
        let so: Vec<f64> = rows
            .iter()
            .map(|r| {
                r["all_variant"]["so_bytes"].as_f64().unwrap_or(0.0)
                    / r["skelot"]["so_bytes"].as_f64().unwrap_or(1.0).max(1.0)
            })
            .collect();
        let ex: Vec<f64> = rows
            .iter()
            .filter_map(|r| {
                let av = r["all_variant"]["exec_median_ns"].as_f64()?;
                let sk = r["skelot"]["exec_median_ns"].as_f64()?;
                if sk > 0.0 {
                    Some(av / sk)
                } else {
                    None
                }
            })
            .collect();
        let mut ps: Vec<f64> = rows
            .iter()
            .filter_map(|r| r["all_variant"]["welch_p_vs_skelot"].as_f64())
            .filter(|v| !v.is_nan())
            .collect();
        ps.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p_med = if ps.is_empty() { 1.0 } else { ps[ps.len() / 2] };

        aggregates.push(serde_json::json!({
            "bucket": bucket,
            "n": rows.len(),
            "compile_time_geomean_ratio_allvariant_over_skelot": geomean(&ct),
            "so_bytes_geomean_ratio_allvariant_over_skelot": geomean(&so),
            "exec_median_geomean_ratio_allvariant_over_skelot":
                if ex.is_empty() { 0.0 } else { geomean(&ex) },
            "welch_p_median": p_med,
        }));
    }

    let out = serde_json::json!({
        "meta": {
            "git_sha": std::env::var("GIT_SHA").unwrap_or_else(|_| "unknown".into()),
            "ts_utc": chrono::Utc::now().to_rfc3339(),
            "corpus_source": args.families.display().to_string(),
            "sample_size": sampled.len(),
            "per_bucket": args.per_bucket,
            "rounds": args.rounds,
            "warmup": args.warmup,
            "alpha": args.alpha,
            "compile_members_cap": args.compile_members_cap,
        },
        "families": fam_rows,
        "bucket_aggregates": aggregates,
    });
    std::fs::write(&args.out, serde_json::to_string_pretty(&out)?)?;
    eprintln!("Wrote output to {}", args.out.display());
    Ok(())
}
