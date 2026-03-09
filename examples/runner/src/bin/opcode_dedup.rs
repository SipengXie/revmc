//! Analyze EVM bytecode deduplication after stripping PUSH immediates.
//!
//! EVM bytecode mixes opcodes with constant data (PUSH operands). This tool:
//! 1. Scans all state files to collect unique bytecodes (by code_hash)
//! 2. Strips PUSH1-PUSH32 immediate bytes, keeping only the opcode skeleton
//! 3. Reports deduplication ratio of opcode skeletons
//!
//! Usage:
//!   cargo run -p revmc-examples-runner --bin opcode_dedup --release

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use revm::bytecode::Bytecode;
use revm::primitives::{Address, B256, HashMap as RevmHashMap, U256};
use revm::state::AccountInfo;
use serde::Deserialize;

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

/// Extract the opcode skeleton from EVM bytecode.
/// Keeps all opcodes but replaces PUSH1-PUSH32 immediate bytes with zeros.
/// Returns (skeleton_bytes, opcode_count, immediate_bytes_count).
fn extract_opcode_skeleton(bytes: &[u8]) -> (Vec<u8>, usize, usize) {
    let mut skeleton = Vec::with_capacity(bytes.len());
    let mut opcode_count = 0usize;
    let mut imm_bytes = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        let op = bytes[i];
        skeleton.push(op);
        opcode_count += 1;
        i += 1;
        if op >= 0x60 && op <= 0x7f {
            // PUSH1..PUSH32: skip N immediate bytes
            let n = (op - 0x5f) as usize;
            imm_bytes += n;
            // Don't include immediate bytes in skeleton at all
            // (just keep the PUSHn opcode)
            i += n;
        }
    }
    (skeleton, opcode_count, imm_bytes)
}

/// Hash a byte slice using a simple FNV-like approach via std.
fn hash_bytes(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn main() {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/home/ubuntu/sipeng/bench_data".into());
    let bench_dir = Path::new(&dir);
    let start: u64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(38004930);
    let count: u64 = std::env::args()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(9997);
    let end = start + count;

    println!("=== EVM Bytecode Opcode Skeleton Deduplication Analysis ===");
    println!("Scanning blocks {start}..{end} ({count} blocks)\n");

    let t0 = Instant::now();

    // Phase 1: collect all unique bytecodes by code_hash
    let mut all_codes: HashMap<B256, Vec<u8>> = HashMap::new();
    let mut scanned = 0u64;

    for bn in start..end {
        let path = bench_dir.join(format!("states/{bn}.bin"));
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let snapshot: CacheSnapshot = match bincode::deserialize(&data) {
            Ok(s) => s,
            Err(_) => continue,
        };
        for (hash, bytecode) in snapshot.codes {
            all_codes
                .entry(hash)
                .or_insert_with(|| bytecode.original_byte_slice().to_vec());
        }
        scanned += 1;
        if scanned % 1000 == 0 {
            eprintln!(
                "  {scanned} blocks scanned, {} unique bytecodes so far",
                all_codes.len()
            );
        }
    }

    let scan_elapsed = t0.elapsed();
    let total_unique_bytecodes = all_codes.len();
    let non_empty: HashMap<&B256, &Vec<u8>> = all_codes
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .collect();
    let non_empty_count = non_empty.len();

    println!(
        "Phase 1: Scanned {scanned} blocks in {:.1}s",
        scan_elapsed.as_secs_f64()
    );
    println!("  Total unique bytecodes (by code_hash): {total_unique_bytecodes}");
    println!(
        "  Non-empty: {non_empty_count}  |  Empty: {}",
        total_unique_bytecodes - non_empty_count
    );

    // Phase 2: extract opcode skeletons and deduplicate
    let t1 = Instant::now();

    // skeleton_hash -> (representative code_hash, skeleton_len, count_of_bytecodes_sharing_this_skeleton)
    let mut skeleton_map: HashMap<u64, (B256, usize, usize, Vec<u8>)> = HashMap::new();
    // For each bytecode: code_hash -> skeleton_hash
    let mut code_to_skeleton: HashMap<B256, u64> = HashMap::new();

    let mut total_raw_bytes: usize = 0;
    let mut total_opcode_bytes: usize = 0;
    let mut total_imm_bytes: usize = 0;

    for (hash, bytes) in &all_codes {
        if bytes.is_empty() {
            continue;
        }
        let (skeleton, opcode_count, imm_count) = extract_opcode_skeleton(bytes);
        total_raw_bytes += bytes.len();
        total_opcode_bytes += skeleton.len();
        total_imm_bytes += imm_count;

        let skel_hash = hash_bytes(&skeleton);
        code_to_skeleton.insert(*hash, skel_hash);

        skeleton_map
            .entry(skel_hash)
            .and_modify(|(_, _, count, _)| *count += 1)
            .or_insert((*hash, skeleton.len(), 1, skeleton));
    }

    let unique_skeletons = skeleton_map.len();
    let phase2_elapsed = t1.elapsed();

    println!(
        "\nPhase 2: Skeleton extraction in {:.1}s",
        phase2_elapsed.as_secs_f64()
    );
    println!("\n=== Raw bytecode composition ===");
    println!(
        "  Total raw bytecode: {:.2} MB ({total_raw_bytes} bytes)",
        total_raw_bytes as f64 / 1_048_576.0
    );
    println!(
        "  Opcode bytes (skeleton): {:.2} MB ({total_opcode_bytes} bytes, {:.1}%)",
        total_opcode_bytes as f64 / 1_048_576.0,
        total_opcode_bytes as f64 / total_raw_bytes as f64 * 100.0
    );
    println!(
        "  Immediate bytes (PUSH data): {:.2} MB ({total_imm_bytes} bytes, {:.1}%)",
        total_imm_bytes as f64 / 1_048_576.0,
        total_imm_bytes as f64 / total_raw_bytes as f64 * 100.0
    );
    let trailing = total_raw_bytes as isize - total_opcode_bytes as isize - total_imm_bytes as isize;
    if trailing > 0 {
        println!(
            "  Trailing/unreachable data: {} bytes ({:.1}%)",
            trailing,
            trailing as f64 / total_raw_bytes as f64 * 100.0
        );
    }

    println!("\n=== Deduplication results ===");
    println!("  Unique bytecodes (by code_hash): {non_empty_count}");
    println!("  Unique opcode skeletons:         {unique_skeletons}");
    println!(
        "  Dedup ratio: {:.1}x  ({} bytecodes collapsed into {} skeletons, {:.1}% reduction)",
        non_empty_count as f64 / unique_skeletons as f64,
        non_empty_count,
        unique_skeletons,
        (1.0 - unique_skeletons as f64 / non_empty_count as f64) * 100.0
    );

    // Phase 3: analyze skeleton duplication distribution
    let mut dup_counts: Vec<usize> = skeleton_map.values().map(|(_, _, c, _)| *c).collect();
    dup_counts.sort_unstable_by(|a, b| b.cmp(a));

    let singletons = dup_counts.iter().filter(|&&c| c == 1).count();
    let duplicated = unique_skeletons - singletons;

    println!("\n=== Skeleton duplication distribution ===");
    println!(
        "  Skeletons appearing exactly once: {singletons} ({:.1}%)",
        singletons as f64 / unique_skeletons as f64 * 100.0
    );
    println!(
        "  Skeletons shared by 2+ bytecodes: {duplicated} ({:.1}%)",
        duplicated as f64 / unique_skeletons as f64 * 100.0
    );

    // Top duplicated skeletons
    println!("\n  Top 20 most duplicated skeletons:");
    println!("  {:>6}  {:>8}  {}", "copies", "skel_len", "representative_hash");
    let mut entries: Vec<_> = skeleton_map.values().collect();
    entries.sort_by_key(|(_, _, c, _)| std::cmp::Reverse(*c));
    for (repr_hash, skel_len, count, _skeleton) in entries.iter().take(20) {
        println!(
            "  {:>6}  {:>7}B  {}",
            count,
            skel_len,
            &hex::encode(repr_hash.as_slice())[..16]
        );
    }

    // Size-bucketed dedup analysis
    println!("\n=== Size-bucketed dedup ===");
    let buckets = [
        ("tiny (<100B)", 0usize, 100usize),
        ("small (100B-500B)", 100, 500),
        ("medium (500B-5KB)", 500, 5000),
        ("large (5KB-15KB)", 5000, 15000),
        ("huge (>15KB)", 15000, usize::MAX),
    ];

    for (label, lo, hi) in &buckets {
        let codes_in_bucket: Vec<_> = all_codes
            .iter()
            .filter(|(_, v)| !v.is_empty() && v.len() >= *lo && v.len() < *hi)
            .collect();
        let n_codes = codes_in_bucket.len();
        if n_codes == 0 {
            println!("  {label}: 0 bytecodes");
            continue;
        }
        let skeleton_hashes: std::collections::HashSet<u64> = codes_in_bucket
            .iter()
            .filter_map(|(h, _)| code_to_skeleton.get(*h))
            .copied()
            .collect();
        let n_skels = skeleton_hashes.len();
        println!(
            "  {label}: {n_codes} bytecodes -> {n_skels} skeletons ({:.1}x dedup, {:.1}% reduction)",
            n_codes as f64 / n_skels as f64,
            (1.0 - n_skels as f64 / n_codes as f64) * 100.0
        );
    }

    // Byte savings analysis
    println!("\n=== Potential code sharing savings ===");
    let mut shared_bytes_saved: usize = 0;
    for (_skel_hash, (_repr, skel_len, count, _)) in &skeleton_map {
        if *count > 1 {
            // (count - 1) copies are redundant
            shared_bytes_saved += (*count - 1) * skel_len;
        }
    }
    println!(
        "  If skeleton-identical contracts shared compiled code:");
    println!(
        "    Redundant opcode bytes: {:.2} MB ({shared_bytes_saved} bytes)",
        shared_bytes_saved as f64 / 1_048_576.0
    );
    println!(
        "    Percentage of total opcode bytes: {:.1}%",
        shared_bytes_saved as f64 / total_opcode_bytes as f64 * 100.0
    );

    let total_elapsed = t0.elapsed();
    println!(
        "\nTotal time: {:.1}s",
        total_elapsed.as_secs_f64()
    );
}
