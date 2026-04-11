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
use revm::bytecode::Bytecode;
use revm::primitives::{Address, B256, HashMap as RevmHashMap, U256};
use revm::state::AccountInfo;
use serde::{Deserialize, Serialize};

use crate::skelot_common::{
    extract_opcode_skeleton_with_counts, hash_bytes, identify_pattern, should_skip_bytes,
};

#[derive(Parser)]
#[command(name = "skelot_dataset_scan")]
struct Args {
    #[arg(long)]
    bench_dir: String,
    #[arg(long, default_value_t = 38004930)]
    start_block: u64,
    #[arg(long, default_value_t = 10000)]
    block_count: u64,
    #[arg(long, default_value_t = 1000)]
    window_size: u64,
    #[arg(long)]
    out_dir: String,
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
}

#[derive(Serialize)]
struct DatasetScan {
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

fn bucket_for_raw_len(len: usize) -> &'static str {
    match len {
        0..=99 => "tiny",
        100..=499 => "small",
        500..=4_999 => "medium",
        5_000..=14_999 => "large",
        _ => "huge",
    }
}

struct ScanContext {
    /// Global dedup map: code_hash -> raw bytecode bytes.
    codes: HashMap<B256, Vec<u8>>,
    /// Per-code skeleton hash so windows can reuse work.
    skeleton_of: HashMap<B256, u64>,
    /// Per-skeleton member set for final family enumeration.
    skeleton_members: HashMap<u64, (usize /*skel_len*/, Vec<B256>)>,
}

impl ScanContext {
    fn new() -> Self {
        Self {
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
        let (skel, _ops, _imm) = extract_opcode_skeleton_with_counts(&bytecode_bytes);
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

fn scan_window(bench_dir: &PathBuf, start: u64, count: u64, ctx: &mut ScanContext) {
    for bn in start..start + count {
        let path = bench_dir.join(format!("states/{bn}.bin"));
        let Ok(data) = fs::read(&path) else { continue };
        let Ok(snap): Result<CacheSnapshot, _> = bincode::deserialize(&data) else { continue };
        for (hash, bytecode) in snap.codes {
            let raw = bytecode.original_byte_slice().to_vec();
            ctx.ingest(raw, hash);
        }
    }
}

fn metrics_for_hashes(
    hashes: impl IntoIterator<Item = B256>,
    ctx: &ScanContext,
) -> ScopeMetrics {
    let mut bytecodes_total = 0usize;
    let mut raw_bytes_total = 0usize;
    let mut imm_bytes_total = 0usize;
    let mut opcode_bytes_total = 0usize;
    let mut skeleton_set: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut buckets: HashMap<
        String,
        (usize /*bc*/, std::collections::HashSet<u64> /*skel set*/, usize /*raw bytes sum*/),
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
        let entry = buckets
            .entry(bucket)
            .or_insert_with(|| (0, std::collections::HashSet::new(), 0));
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
            let ratio = if bc == 0 {
                0.0
            } else {
                1.0 - skel_set.len() as f64 / bc as f64
            };
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
    let bench_dir = PathBuf::from(&args.bench_dir);
    let out_dir = PathBuf::from(&args.out_dir);
    fs::create_dir_all(&out_dir).expect("create out dir");

    let n_windows = args.block_count / args.window_size;
    assert!(n_windows >= 1, "block_count must be ≥ window_size");
    eprintln!(
        "Stage A: scanning {} blocks from {} in {} windows of {}",
        args.block_count, args.start_block, n_windows, args.window_size
    );

    let t0 = Instant::now();
    let mut ctx = ScanContext::new();
    // Track which hashes first appeared in which window, so per-window metrics
    // include only the bytecodes newly seen in that window.
    let mut per_window_hashes: Vec<Vec<B256>> = vec![Vec::new(); n_windows as usize];

    for w in 0..n_windows {
        let start = args.start_block + w * args.window_size;
        let before: std::collections::HashSet<B256> = ctx.codes.keys().copied().collect();
        scan_window(&bench_dir, start, args.window_size, &mut ctx);
        let new_hashes: Vec<B256> = ctx
            .codes
            .keys()
            .copied()
            .filter(|h| !before.contains(h))
            .collect();
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
            start_block: args.start_block + (i as u64) * args.window_size,
            end_block: args.start_block + ((i + 1) as u64) * args.window_size,
            metrics: metrics_for_hashes(hashes.iter().copied(), &ctx),
        })
        .collect();

    let scan = DatasetScan { corpus, windows };
    let scan_path = out_dir.join("dataset_scan.json");
    fs::write(&scan_path, serde_json::to_vec_pretty(&scan).expect("serialize"))
        .expect("write dataset_scan.json");
    eprintln!("wrote {}", scan_path.display());

    // ── families ────────────────────────────────────────────────────────
    let mut families: Vec<(u64, usize, Vec<B256>)> = ctx
        .skeleton_members
        .into_iter()
        .map(|(k, (skel_len, v))| (k, skel_len, v))
        .collect();
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
