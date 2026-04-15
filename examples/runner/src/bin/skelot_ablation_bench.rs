#[path = "../bin_common.rs"]
#[allow(dead_code)]
mod bin_common;

use clap::Parser;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[path = "../skelot_common.rs"]
#[allow(dead_code)]
mod skelot_common;

use skelot_common::{stratified_family_sample, FamilySummary};

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
    /// Output JSON path.
    #[arg(long)]
    out: PathBuf,
}

#[derive(Debug, Deserialize, Serialize)]
struct StageAFamily {
    #[allow(dead_code)]
    rank: usize,
    skeleton_hash: String,
    skeleton_len: usize,
    family_size: usize,
    #[allow(dead_code)]
    bucket: String,
    #[serde(default)]
    #[allow(dead_code)]
    protocol_tag: Option<String>,
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
        .map(|f| FamilySummary {
            skeleton_id: f.skeleton_hash.clone(),
            skeleton_raw_len: f.skeleton_len,
            member_count: f.family_size,
            member_hashes: by_skel.remove(&f.skeleton_hash).unwrap_or_default(),
        })
        .collect())
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let families = load_stage_a_families(&args.families)?;
    let sampled = stratified_family_sample(&families, args.per_bucket, args.seed);
    eprintln!("Sampled {} families from {} total", sampled.len(), families.len());
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
        },
        "families": [],
        "bucket_aggregates": [],
    });
    std::fs::write(&args.out, serde_json::to_string_pretty(&out)?)?;
    eprintln!("Wrote scaffold output to {}", args.out.display());
    Ok(())
}
