//! JSON output schema and effect-size helper for the skeleton-vs-per-hash
//! comparison report. Split out of `skeleton_vs_perhash_bench.rs`.

use serde::Serialize;

#[derive(Serialize)]
pub struct Config {
    pub blocks_sampled: usize,
    pub rounds: usize,
    pub warmup: usize,
    pub alpha: f64,
}

#[derive(Serialize)]
pub struct Summary {
    pub total_family_members_measured: usize,
    pub skeleton_faster: usize,
    pub equivalent: usize,
    pub skeleton_slower: usize,
    pub median_speedup_ratio: f64,
    pub mean_delta_us: f64,
    pub p95_delta_us: f64,
    /// Blocks the per-tx (success, gas) gate rejected, and the total number of
    /// differing transactions in them. A run that reports non-zero here has NOT
    /// measured those blocks in any arm — without this the gate could only be
    /// observed on stderr.
    pub correctness_blocks_rejected: usize,
    pub correctness_tx_mismatches: u64,
}

#[derive(Serialize)]
pub struct RawSample {
    pub block: u64,
    pub round: usize,
    pub per_hash_us: f64,
    pub skeleton_us: f64,
}

#[derive(Serialize)]
pub struct FamilyAggregate {
    pub skeleton_hash: String,
    pub family_size: usize,
    pub members_measured: usize,
    pub median_speedup: f64,
    pub mean_delta_us: f64,
    pub pct_faster: f64,
    pub pct_equivalent: f64,
    pub pct_slower: f64,
}

#[derive(Serialize)]
pub struct BlockSummary {
    pub block: u64,
    pub contracts_measured: usize,
    pub median_speedup: f64,
    pub mean_delta_us: f64,
}

#[derive(Serialize)]
pub struct ContractRecord {
    pub code_hash: String,
    pub skeleton_hash: String,
    pub family_size: usize,
    /// Number of (block, round) samples backing this record — NOT distinct
    /// blocks (kept under the historic name for downstream compatibility).
    pub blocks_seen: usize,
    pub mean_per_hash_us: f64,
    pub mean_skeleton_us: f64,
    pub delta_us: f64,
    pub speedup_ratio: f64,
    pub welch_p: f64,
    pub cohens_d: f64,
    pub verdict: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub raw_samples: Vec<RawSample>,
}

/// Per-target aggregate for one mode (unconditional, also in single-mode
/// runs where the paired per_contract analysis is empty).
#[derive(Serialize)]
pub struct TargetAggregate {
    pub code_hash: String,
    pub samples: usize,
    pub total_us: f64,
}

/// One arm's own report: exec wall clock, replay totals for offline
/// cross-arm result diffing, and per-target timing aggregates.
#[derive(Serialize)]
pub struct ModeReport {
    pub mode: String,
    /// Sum of per-block tx-loop execution time (loader IO and CacheDB
    /// construction excluded), measured-rounds only.
    pub exec_wall_seconds: f64,
    pub blocks_replayed: usize,
    pub blocks_skipped_unreadable: usize,
    /// Replay totals from round 0 (success flag / gas per tx summed).
    pub tx_count: u64,
    pub success_count: u64,
    pub total_gas: u64,
    pub samples_total: usize,
    pub targets_hit: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub masked_jump_hits: Option<u64>,
    pub per_target: Vec<TargetAggregate>,
}

#[derive(Serialize)]
pub struct Output {
    pub config: Config,
    pub summary: Summary,
    pub mode_reports: Vec<ModeReport>,
    pub per_family: Vec<FamilyAggregate>,
    pub per_block: Vec<BlockSummary>,
    pub per_contract: Vec<ContractRecord>,
}

pub fn cohens_d(a: &[f64], b: &[f64]) -> f64 {
    let mean_a = a.iter().sum::<f64>() / a.len() as f64;
    let mean_b = b.iter().sum::<f64>() / b.len() as f64;
    let var_a =
        a.iter().map(|x| (x - mean_a).powi(2)).sum::<f64>() / (a.len() as f64 - 1.0).max(1.0);
    let var_b =
        b.iter().map(|x| (x - mean_b).powi(2)).sum::<f64>() / (b.len() as f64 - 1.0).max(1.0);
    let pooled_std = ((var_a + var_b) / 2.0).sqrt();
    if pooled_std == 0.0 {
        0.0
    } else {
        (mean_a - mean_b) / pooled_std
    }
}


