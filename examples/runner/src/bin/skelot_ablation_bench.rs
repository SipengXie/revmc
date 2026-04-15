#[path = "../bin_common.rs"]
#[allow(dead_code)]
mod bin_common;

use clap::Parser;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Instant;

#[path = "../skelot_common.rs"]
#[allow(dead_code)]
mod skelot_common;

use revm::primitives::B256;
use revmc::skeleton::{analyze_skeleton_group_with_mode, ClassifyMode};
use revmc::OptimizationLevel;
use skelot_common::{
    collect_bytecodes, compile_per_hash, compile_skeleton_group, stratified_family_sample,
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
    /// Welch alpha.
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
    /// Max resolved bytecodes per family for the compile phase. per_hash cost
    /// scales linearly with this; SkelOT / all_variant compile once per family
    /// regardless. Truncation is deterministic (sorted by hash).
    #[arg(long, default_value_t = 128)]
    compile_members_cap: usize,
    /// Output JSON path.
    #[arg(long)]
    out: PathBuf,
}

#[derive(Debug, Deserialize)]
struct StageAFamily {
    skeleton_hash: String,
    skeleton_len: usize,
    family_size: usize,
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
    let mut by_skel: std::collections::HashMap<String, Vec<String>> = Default::default();
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
                skeleton_raw_len: f.skeleton_len,
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

fn load_family_bytecodes(
    all_codes: &std::collections::HashMap<B256, Vec<u8>>,
    fam: &FamilySummary,
    cap: usize,
) -> Vec<Vec<u8>> {
    use std::str::FromStr;
    let mut resolved: Vec<(String, Vec<u8>)> = fam
        .member_hashes
        .iter()
        .filter_map(|h| {
            let key = B256::from_str(h.trim_start_matches("0x")).ok()?;
            let code = all_codes.get(&key)?;
            Some((h.clone(), code.clone()))
        })
        .collect();
    resolved.sort_by(|a, b| a.0.cmp(&b.0));
    resolved.truncate(cap);
    resolved.into_iter().map(|(_, v)| v).collect()
}

fn compile_per_hash_mode(
    bytecodes: &[Vec<u8>],
    tmp_dir: &std::path::Path,
) -> anyhow::Result<CompileMetrics> {
    let t = Instant::now();
    let mut so_bytes = 0u64;
    for (i, bc) in bytecodes.iter().enumerate() {
        let obj = tmp_dir.join(format!("per_hash_{i}.o"));
        let lib = tmp_dir.join(format!("per_hash_{i}.so"));
        compile_per_hash(bc, &format!("ph_{i}"), &obj, &lib, OPT)
            .map_err(|e| anyhow::anyhow!("compile_per_hash: {e}"))?;
        so_bytes += std::fs::metadata(&lib)?.len();
    }
    let total_push = bytecodes.first().map(|bc| count_push_immediates(bc) as u32).unwrap_or(0);
    Ok(CompileMetrics {
        compile_time_ms: t.elapsed().as_secs_f64() * 1000.0,
        so_bytes,
        num_variant: 0,
        num_invariant: total_push,
        num_skip_logic_preserved: 0,
    })
}

fn compile_skeleton_mode(
    bytecodes: &[Vec<u8>],
    mode: ClassifyMode,
    tmp_dir: &std::path::Path,
    tag: &str,
) -> anyhow::Result<(CompileMetrics, revmc::skeleton::SkeletonVariance)> {
    let refs: Vec<&[u8]> = bytecodes.iter().map(|v| v.as_slice()).collect();
    let variance = analyze_skeleton_group_with_mode(&refs, mode);
    let obj = tmp_dir.join(format!("{tag}.o"));
    let lib = tmp_dir.join(format!("{tag}.so"));
    let t = Instant::now();
    compile_skeleton_group(&bytecodes[0], &variance, tag, &obj, &lib, OPT)
        .map_err(|e| anyhow::anyhow!("compile_skeleton_group: {e}"))?;
    let ct = t.elapsed().as_secs_f64() * 1000.0;
    let so_bytes = std::fs::metadata(&lib)?.len();
    let num_total = variance.pushes.len() as u32;
    let num_invariant = num_total - variance.num_variant;
    // Only ForceAllVariant keeps PUSHes Invariant *because* of SKIP_LOGIC.
    // In Auto mode an Invariant may also come from cross-family equality, so
    // we cannot attribute them to SKIP_LOGIC here — report 0.
    let num_skip_logic_preserved = match mode {
        ClassifyMode::ForceAllVariant => num_invariant,
        ClassifyMode::Auto => 0,
    };
    Ok((
        CompileMetrics {
            compile_time_ms: ct,
            so_bytes,
            num_variant: variance.num_variant,
            num_invariant,
            num_skip_logic_preserved,
        },
        variance,
    ))
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let families = load_stage_a_families(&args.families)?;
    let sampled = stratified_family_sample(&families, args.per_bucket, args.seed);
    eprintln!("Sampled {} families from {} total", sampled.len(), families.len());

    let all_codes = collect_bytecodes(&args.bench_dir, args.start_block, args.block_count);
    eprintln!(
        "Collected {} compilable bytecodes from states/[{}..{})",
        all_codes.len(),
        args.start_block,
        args.start_block + args.block_count
    );

    let tmp_root = tempfile::tempdir()?;
    let mut fam_rows = Vec::<serde_json::Value>::new();
    for (idx, fam) in sampled.iter().enumerate() {
        let bytecodes = load_family_bytecodes(&all_codes, fam, args.compile_members_cap);
        if bytecodes.is_empty() {
            eprintln!("[skip] family {}: no bytecodes resolved from corpus", fam.skeleton_id);
            continue;
        }

        let fam_tmp = tmp_root.path().join(&fam.skeleton_id);
        std::fs::create_dir_all(&fam_tmp)?;

        let per_hash = compile_per_hash_mode(&bytecodes, &fam_tmp)?;
        let (skelot, _var_auto) =
            compile_skeleton_mode(&bytecodes, ClassifyMode::Auto, &fam_tmp, "skelot_auto")?;
        let (all_variant, _var_forced) = compile_skeleton_mode(
            &bytecodes,
            ClassifyMode::ForceAllVariant,
            &fam_tmp,
            "skelot_forced",
        )?;

        let bucket_name = match skelot_common::bucket_for_raw_len(fam.skeleton_raw_len) {
            0 => "tiny",
            1 => "small",
            2 => "medium",
            3 => "large",
            _ => "huge",
        };

        fam_rows.push(serde_json::json!({
            "skeleton_id": fam.skeleton_id,
            "bucket": bucket_name,
            "member_count": fam.member_count,
            "members_compiled": bytecodes.len(),
            "per_hash": { "compile": per_hash },
            "skelot": { "compile": skelot },
            "all_variant": { "compile": all_variant },
        }));
        eprintln!("[{}/{}] {} compiled", idx + 1, sampled.len(), fam.skeleton_id);
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
        "bucket_aggregates": [],
    });
    std::fs::write(&args.out, serde_json::to_string_pretty(&out)?)?;
    eprintln!("Wrote output to {}", args.out.display());
    Ok(())
}
