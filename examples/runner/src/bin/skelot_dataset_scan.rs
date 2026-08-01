//! Stage A — SkelOT paper dataset scanner.

#[path = "../bin_common.rs"]
#[allow(dead_code)]
mod bin_common;

#[path = "../skelot_common.rs"]
#[allow(dead_code)]
mod skelot_common;

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;
use revm::primitives::B256;
use revmc::emse::SkeletonMode;
use serde::Serialize;

use crate::skelot_common::{
    bucket_for_raw_len, extract_emse_skeleton, extract_opcode_skeleton_with_counts, hash_bytes,
    identify_pattern, should_skip_bytes, BUCKET_SCHEME_ID,
};

#[derive(Parser)]
#[command(name = "skelot_dataset_scan")]
struct Args {
    #[arg(long, default_value = "base")]
    chain: String,
    #[arg(long)]
    bench_dir: String,
    #[arg(long)]
    start_block: Option<u64>,
    #[arg(long)]
    block_count: Option<u64>,
    #[arg(long, default_value_t = 1000)]
    window_size: u64,
    #[arg(long, default_value = "current")]
    skeleton_mode: SkeletonMode,
    #[arg(long)]
    out_dir: String,
}

#[derive(Serialize, Default, Clone)]
struct BucketMetric {
    count_bytecodes: usize,
    count_skeletons: usize,
    reduction_ratio: f64,
    mean_size: f64,
    raw_bytes: usize,
}

#[derive(Serialize, Default, Clone)]
struct ScopeMetrics {
    unique_bytecodes: usize,
    unique_skeletons: usize,
    reduction_ratio: f64,
    push_immediate_bytes: usize,
    total_opcode_bytes: usize,
    raw_bytes: usize,
    buckets: HashMap<String, BucketMetric>,
}

#[derive(Serialize)]
struct WindowRecord {
    window_id: usize,
    start_block: u64,
    end_block: u64,
    metrics: ScopeMetrics,
    metrics_local: ScopeMetrics,
}

#[derive(Serialize)]
struct DatasetScan {
    chain: String,
    bucket_scheme: &'static str,
    schema_version: u32,
    start_block: u64,
    block_count: u64,
    window_size: u64,
    corpus: ScopeMetrics,
    windows: Vec<WindowRecord>,
}

#[derive(Serialize)]
struct Family {
    rank: usize,
    skeleton_hash: String,
    skeleton_len: usize,
    family_size: usize,
    bucket: String,
    protocol_tag: Option<String>,
}

#[derive(Serialize)]
struct FamilyFile {
    families: Vec<Family>,
}

struct ScanContext {
    skeleton_mode: SkeletonMode,
    /// Global dedup map: code_hash -> raw bytecode bytes.
    codes: HashMap<B256, Vec<u8>>,
    /// Per-code skeleton hash so windows can reuse work.
    skeleton_of: HashMap<B256, u64>,
    /// Per-skeleton member set for final family enumeration.
    skeleton_members: HashMap<u64, (usize /*skel_len*/, Vec<B256>)>,
}

impl ScanContext {
    fn new(skeleton_mode: SkeletonMode) -> Self {
        Self {
            skeleton_mode,
            codes: HashMap::new(),
            skeleton_of: HashMap::new(),
            skeleton_members: HashMap::new(),
        }
    }

    fn ingest(&mut self, bytecode_bytes: Vec<u8>, hash: B256) {
        if should_skip_bytes(&bytecode_bytes) {
            return;
        }
        if self.codes.contains_key(&hash) {
            return;
        }
        let skel = match self.skeleton_mode {
            SkeletonMode::Current => extract_opcode_skeleton_with_counts(&bytecode_bytes).0,
            SkeletonMode::Emse => extract_emse_skeleton(&bytecode_bytes),
        };
        let skel_len = skel.len();
        let skel_hash = hash_bytes(&skel);
        self.codes.insert(hash, bytecode_bytes);
        self.skeleton_of.insert(hash, skel_hash);
        self.skeleton_members
            .entry(skel_hash)
            .or_insert_with(|| (skel_len, Vec::new()))
            .1
            .push(hash);
    }
}

/// Ingests every block in `[start, start + count)` and returns the set of
/// bytecode hashes seen anywhere in the window (not just the newly-added ones).
fn scan_window(
    bench_dir: &PathBuf,
    start: u64,
    count: u64,
    ctx: &mut ScanContext,
) -> std::collections::HashSet<B256> {
    let mut seen = std::collections::HashSet::new();
    for bn in start..start + count {
        let path = bench_dir.join(format!("states/{bn}.bin"));
        let snap = match crate::skelot_common::load_state_v1(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        crate::skelot_common::iter_bytecode_raw(&snap, |hash, raw| {
            seen.insert(*hash);
            ctx.ingest(raw, *hash);
        });
    }
    seen
}

fn metrics_for_hashes(hashes: impl IntoIterator<Item = B256>, ctx: &ScanContext) -> ScopeMetrics {
    let mut bytecodes_total = 0usize;
    let mut raw_bytes_total = 0usize;
    let mut imm_bytes_total = 0usize;
    let mut opcode_bytes_total = 0usize;
    let mut skeleton_set: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut buckets: HashMap<
        String,
        (
            usize,                          /*bc*/
            std::collections::HashSet<u64>, /*skel set*/
            usize,                          /*raw bytes sum*/
        ),
    > = HashMap::new();

    for hash in hashes {
        let Some(bytes) = ctx.codes.get(&hash) else { continue };
        let Some(skel_hash) = ctx.skeleton_of.get(&hash).copied() else { continue };
        bytecodes_total += 1;
        let raw_len = bytes.len();
        raw_bytes_total += raw_len;
        let (_, ops, imm) = extract_opcode_skeleton_with_counts(bytes);
        opcode_bytes_total += ops;
        imm_bytes_total += imm;
        skeleton_set.insert(skel_hash);
        let bucket = bucket_for_raw_len(raw_len).to_string();
        let entry =
            buckets.entry(bucket).or_insert_with(|| (0, std::collections::HashSet::new(), 0));
        entry.0 += 1;
        entry.1.insert(skel_hash);
        entry.2 += raw_len;
    }

    let reduction_ratio = if bytecodes_total == 0 {
        0.0
    } else {
        1.0 - (skeleton_set.len() as f64 / bytecodes_total as f64)
    };

    let bucket_metrics: HashMap<String, BucketMetric> = buckets
        .into_iter()
        .map(|(name, (bc, skel_set, raw_sum))| {
            let ratio = if bc == 0 { 0.0 } else { 1.0 - skel_set.len() as f64 / bc as f64 };
            let mean = if bc == 0 { 0.0 } else { raw_sum as f64 / bc as f64 };
            (
                name,
                BucketMetric {
                    count_bytecodes: bc,
                    count_skeletons: skel_set.len(),
                    reduction_ratio: ratio,
                    mean_size: mean,
                    raw_bytes: raw_sum,
                },
            )
        })
        .collect();

    ScopeMetrics {
        unique_bytecodes: bytecodes_total,
        unique_skeletons: skeleton_set.len(),
        reduction_ratio,
        push_immediate_bytes: imm_bytes_total,
        total_opcode_bytes: opcode_bytes_total,
        raw_bytes: raw_bytes_total,
        buckets: bucket_metrics,
    }
}

fn main() {
    let args = Args::parse();
    let chain: skelot_common::ChainId = args.chain.parse().expect("invalid --chain");
    let (default_start, default_count) = match chain {
        skelot_common::ChainId::Base => (38_004_930u64, 10_000u64),
        skelot_common::ChainId::Eth => (23_000_000, 1_000),
        skelot_common::ChainId::Bsc => (95_897_884, 1_000),
        skelot_common::ChainId::Arb => (458_863_320, 10_000),
    };
    let start_block = args.start_block.unwrap_or(default_start);
    let block_count = args.block_count.unwrap_or(default_count);

    let bench_dir = PathBuf::from(&args.bench_dir);
    let out_dir = PathBuf::from(&args.out_dir);
    fs::create_dir_all(&out_dir).expect("create out dir");

    // Ceiling division: a trailing partial window scans the remainder blocks
    // instead of silently dropping them from the corpus.
    let n_windows = block_count.div_ceil(args.window_size);
    assert!(n_windows >= 1, "block_count must be ≥ 1");
    eprintln!(
        "Stage A: scanning {} blocks from {} in {} windows of {} (chain={}, skeleton_mode={})",
        block_count,
        start_block,
        n_windows,
        args.window_size,
        chain.as_lowercase(),
        args.skeleton_mode.as_str(),
    );

    let t0 = Instant::now();
    let mut ctx = ScanContext::new(args.skeleton_mode);
    // Track which hashes first appeared in which window, so per-window metrics
    // include only the bytecodes newly seen in that window.
    let mut per_window_hashes: Vec<Vec<B256>> = vec![Vec::new(); n_windows as usize];
    // All hashes seen in each window, captured during the single ingest pass.
    let mut per_window_hashes_local: Vec<Vec<B256>> = vec![Vec::new(); n_windows as usize];

    for w in 0..n_windows {
        let start = start_block + w * args.window_size;
        let window_blocks = args.window_size.min(start_block + block_count - start);
        let before: std::collections::HashSet<B256> = ctx.codes.keys().copied().collect();
        let seen = scan_window(&bench_dir, start, window_blocks, &mut ctx);
        per_window_hashes_local[w as usize] =
            seen.into_iter().filter(|h| ctx.codes.contains_key(h)).collect();
        let new_hashes: Vec<B256> =
            ctx.codes.keys().copied().filter(|h| !before.contains(h)).collect();
        eprintln!(
            "  [window {}/{}] {} new bytecodes ({:.1}s)",
            w + 1,
            n_windows,
            new_hashes.len(),
            t0.elapsed().as_secs_f64(),
        );
        per_window_hashes[w as usize] = new_hashes;
    }


    let all_hashes: Vec<B256> = ctx.codes.keys().copied().collect();
    let corpus = metrics_for_hashes(all_hashes.clone(), &ctx);

    let windows: Vec<WindowRecord> = per_window_hashes
        .iter()
        .enumerate()
        .map(|(i, hashes)| WindowRecord {
            window_id: i,
            start_block: start_block + (i as u64) * args.window_size,
            end_block: start_block + ((i + 1) as u64) * args.window_size,
            metrics: metrics_for_hashes(hashes.iter().copied(), &ctx),
            metrics_local: metrics_for_hashes(per_window_hashes_local[i].iter().copied(), &ctx),
        })
        .collect();

    let schema_version =
        skelot_common::read_bench_manifest(&bench_dir).map(|m| m.schema_version).unwrap_or(0);
    let scan = DatasetScan {
        chain: chain.as_lowercase().into(),
        bucket_scheme: BUCKET_SCHEME_ID,
        schema_version,
        start_block,
        block_count,
        window_size: args.window_size,
        corpus,
        windows,
    };
    let scan_path = out_dir.join("dataset_scan.json");
    fs::write(&scan_path, serde_json::to_vec_pretty(&scan).expect("serialize"))
        .expect("write dataset_scan.json");
    eprintln!("wrote {}", scan_path.display());

    // ── families ────────────────────────────────────────────────────────
    let mut families: Vec<(u64, usize, Vec<B256>)> =
        ctx.skeleton_members.into_iter().map(|(k, (skel_len, v))| (k, skel_len, v)).collect();
    families.sort_by(|a, b| b.2.len().cmp(&a.2.len()).then_with(|| a.0.cmp(&b.0)));

    let families_json: Vec<Family> = families
        .iter()
        .enumerate()
        .map(|(rank, (skel_hash, skel_len, members))| {
            let first_hash = members[0];
            let raw = &ctx.codes[&first_hash];
            let bucket = bucket_for_raw_len(raw.len()).to_string();
            let protocol_tag = if rank < 20 {
                let tag = identify_pattern(raw);
                if tag == "unknown" {
                    None
                } else {
                    Some(tag.to_string())
                }
            } else {
                None
            };
            Family {
                rank,
                skeleton_hash: format!("{:016x}", skel_hash),
                skeleton_len: *skel_len,
                family_size: members.len(),
                bucket,
                protocol_tag,
            }
        })
        .collect();

    let families_file = FamilyFile { families: families_json };
    let fam_path = out_dir.join("skeleton_families.json");
    fs::write(&fam_path, serde_json::to_vec_pretty(&families_file).expect("serialize"))
        .expect("write skeleton_families.json");
    eprintln!("wrote {}", fam_path.display());

    // ── skeleton_mapping.tsv ────────────────────────────────────────────
    let tsv_path = out_dir.join("skeleton_mapping.tsv");
    let mut lines = String::new();
    for (skel_hash, _, members) in &families {
        for member in members {
            lines.push_str(&format!("{:016x}\t0x{}\n", skel_hash, hex::encode(member)));
        }
    }
    fs::write(&tsv_path, lines).expect("write skeleton_mapping.tsv");
    eprintln!("wrote {}", tsv_path.display());

    eprintln!("Stage A done in {:.1}s", t0.elapsed().as_secs_f64());
}

#[cfg(test)]
mod tests {
    #[test]
    fn bucket_boundaries_use_raw_bytecode_length() {
        use crate::skelot_common::extract_opcode_skeleton_with_counts;
        // A 120-byte bytecode with a 20-byte PUSH20 payload should land in
        // the `small` bucket (100..500B), because we bucket by raw length.
        let bc = std::iter::repeat(0x00u8)
            .take(80)
            .chain(std::iter::repeat(0x73u8).take(1))
            .chain(std::iter::repeat(0xFFu8).take(20))
            .chain(std::iter::repeat(0x00u8).take(19))
            .collect::<Vec<u8>>();
        assert_eq!(bc.len(), 120);
        let bucket = super::bucket_for_raw_len(bc.len());
        assert_eq!(bucket, "small");
        let (skel, _, imm) = extract_opcode_skeleton_with_counts(&bc);
        assert_eq!(imm, 20);
        assert!(skel.len() < bc.len());
    }
}
