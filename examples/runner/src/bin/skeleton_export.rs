//! Export skeleton -> bytecodes mapping tables as TSV files.
//!
//! Scans all state files, collects unique bytecodes, computes opcode skeletons
//! (strip PUSH1-PUSH32 immediates), and exports:
//!   - /tmp/skeleton_mapping.tsv   (one row per bytecode)
//!   - /tmp/skeleton_summary.tsv   (one row per skeleton, sorted by copy count desc)
//!
//! Also prints a JIT/AOT cache impact analysis with per-bucket savings.
//!
//! Usage:
//!   cargo run -p revmc-examples-runner --bin skeleton_export --release [bench_dir] [start_block] [block_count]

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::time::Instant;

use revm::bytecode::Bytecode;
use revm::primitives::{Address, B256, HashMap as RevmHashMap, U256};
use revm::state::AccountInfo;
use serde::Deserialize;

// Same structs as scan_codes.rs (standalone, no bin_common.rs dependency)
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
/// Keeps all opcodes but strips PUSH1-PUSH32 immediate bytes.
/// Returns (skeleton_bytes, immediate_bytes_count).
fn extract_opcode_skeleton(bytes: &[u8]) -> (Vec<u8>, usize) {
    let mut skeleton = Vec::with_capacity(bytes.len());
    let mut imm_bytes = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        let op = bytes[i];
        skeleton.push(op);
        i += 1;
        if op >= 0x60 && op <= 0x7f {
            // PUSH1..PUSH32: skip N immediate bytes
            let n = (op - 0x5f) as usize;
            imm_bytes += n;
            i += n;
        }
    }
    (skeleton, imm_bytes)
}

/// Hash a byte slice using DefaultHasher (same as opcode_dedup.rs).
fn hash_bytes(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// Per-bytecode record for the mapping table.
struct CodeRecord {
    code_hash: B256,
    skeleton_hash: u64,
    bytecode_len: usize,
    skeleton_len: usize,
    immediate_bytes: usize,
}

/// Per-skeleton aggregate for the summary table.
struct SkeletonRecord {
    skeleton_hash: u64,
    num_bytecodes: usize,
    skeleton_len: usize,
    bytecode_lens: Vec<usize>,
    example_code_hash: B256,
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

    println!("=== Skeleton Export Tool ===");
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
    println!(
        "Phase 1: Scanned {scanned} blocks in {:.1}s, {} unique bytecodes",
        scan_elapsed.as_secs_f64(),
        all_codes.len()
    );

    // Phase 2: compute skeletons
    let t1 = Instant::now();

    let mut code_records: Vec<CodeRecord> = Vec::new();
    let mut skeleton_agg: HashMap<u64, SkeletonRecord> = HashMap::new();

    for (hash, bytes) in &all_codes {
        if bytes.is_empty() {
            continue;
        }
        let (skeleton, imm_bytes) = extract_opcode_skeleton(bytes);
        let skel_hash = hash_bytes(&skeleton);
        let skel_len = skeleton.len();

        code_records.push(CodeRecord {
            code_hash: *hash,
            skeleton_hash: skel_hash,
            bytecode_len: bytes.len(),
            skeleton_len: skel_len,
            immediate_bytes: imm_bytes,
        });

        skeleton_agg
            .entry(skel_hash)
            .and_modify(|rec| {
                rec.num_bytecodes += 1;
                rec.bytecode_lens.push(bytes.len());
            })
            .or_insert(SkeletonRecord {
                skeleton_hash: skel_hash,
                num_bytecodes: 1,
                skeleton_len: skel_len,
                bytecode_lens: vec![bytes.len()],
                example_code_hash: *hash,
            });
    }

    let phase2_elapsed = t1.elapsed();
    println!(
        "Phase 2: Skeleton extraction in {:.1}s, {} unique skeletons from {} non-empty bytecodes",
        phase2_elapsed.as_secs_f64(),
        skeleton_agg.len(),
        code_records.len()
    );

    // Phase 3: export TSV files
    let t2 = Instant::now();

    // File 1: skeleton_mapping.tsv
    let mapping_path = "/tmp/skeleton_mapping.tsv";
    {
        let mut f = std::fs::File::create(mapping_path).expect("failed to create skeleton_mapping.tsv");
        writeln!(f, "code_hash\tskeleton_hash\tbytecode_len\tskeleton_len\timmediate_bytes")
            .unwrap();
        // Sort by code_hash for deterministic output
        let mut sorted_records: Vec<&CodeRecord> = code_records.iter().collect();
        sorted_records.sort_by_key(|r| r.code_hash);
        for rec in &sorted_records {
            writeln!(
                f,
                "{}\t{:016x}\t{}\t{}\t{}",
                hex::encode(rec.code_hash.as_slice()),
                rec.skeleton_hash,
                rec.bytecode_len,
                rec.skeleton_len,
                rec.immediate_bytes
            )
            .unwrap();
        }
    }

    // File 2: skeleton_summary.tsv
    let summary_path = "/tmp/skeleton_summary.tsv";
    {
        let mut f = std::fs::File::create(summary_path).expect("failed to create skeleton_summary.tsv");
        writeln!(
            f,
            "skeleton_hash\tnum_bytecodes\tskeleton_len\tavg_bytecode_len\tmin_bytecode_len\tmax_bytecode_len\texample_code_hash"
        )
        .unwrap();
        // Sort by num_bytecodes descending
        let mut summaries: Vec<&SkeletonRecord> = skeleton_agg.values().collect();
        summaries.sort_by(|a, b| b.num_bytecodes.cmp(&a.num_bytecodes));
        for rec in &summaries {
            let sum: usize = rec.bytecode_lens.iter().sum();
            let avg = sum as f64 / rec.bytecode_lens.len() as f64;
            let min = *rec.bytecode_lens.iter().min().unwrap();
            let max = *rec.bytecode_lens.iter().max().unwrap();
            writeln!(
                f,
                "{:016x}\t{}\t{}\t{:.1}\t{}\t{}\t{}",
                rec.skeleton_hash,
                rec.num_bytecodes,
                rec.skeleton_len,
                avg,
                min,
                max,
                hex::encode(rec.example_code_hash.as_slice())
            )
            .unwrap();
        }
    }

    let export_elapsed = t2.elapsed();
    println!(
        "\nPhase 3: Exported TSV files in {:.3}s",
        export_elapsed.as_secs_f64()
    );
    println!("  {mapping_path}  ({} rows)", code_records.len());
    println!("  {summary_path}  ({} rows)", skeleton_agg.len());

    // Phase 4: JIT/AOT cache impact analysis
    let total_bytecodes = code_records.len();
    let total_skeletons = skeleton_agg.len();
    let compilations_saved = total_bytecodes - total_skeletons;

    println!("\n=== JIT/AOT Cache Impact Analysis ===");
    println!(
        "  Current approach:        compile each code_hash separately -> {} compilations",
        total_bytecodes
    );
    println!(
        "  Skeleton-aware approach: compile each skeleton once, patch PUSH immediates at load time -> {} compilations",
        total_skeletons
    );
    println!(
        "  Compilation savings:     {} compilations saved ({:.1}% reduction)",
        compilations_saved,
        compilations_saved as f64 / total_bytecodes as f64 * 100.0
    );

    // Per-bucket savings
    let buckets: &[(&str, usize, usize)] = &[
        ("tiny   (<100B)",    0,     100),
        ("small  (100B-500B)",  100,   500),
        ("medium (500B-5KB)",   500,   5000),
        ("large  (5KB-15KB)",   5000,  15000),
        ("huge   (>15KB)",      15000, usize::MAX),
    ];

    // Build a lookup: code_hash -> bytecode_len for non-empty codes
    let code_lens: HashMap<B256, usize> = code_records
        .iter()
        .map(|r| (r.code_hash, r.bytecode_len))
        .collect();

    // Build a lookup: code_hash -> skeleton_hash
    let code_to_skel: HashMap<B256, u64> = code_records
        .iter()
        .map(|r| (r.code_hash, r.skeleton_hash))
        .collect();

    println!("\n  Per-bucket breakdown:");
    println!(
        "  {:24} {:>10} {:>10} {:>10} {:>10}",
        "bucket", "bytecodes", "skeletons", "saved", "reduction%"
    );

    for (label, lo, hi) in buckets {
        let codes_in_bucket: Vec<&B256> = code_lens
            .iter()
            .filter(|(_, &len)| len >= *lo && len < *hi)
            .map(|(h, _)| h)
            .collect();
        let n_codes = codes_in_bucket.len();
        if n_codes == 0 {
            println!("  {:24} {:>10} {:>10} {:>10} {:>10}", label, 0, 0, 0, "N/A");
            continue;
        }
        let unique_skels: std::collections::HashSet<u64> = codes_in_bucket
            .iter()
            .filter_map(|h| code_to_skel.get(*h))
            .copied()
            .collect();
        let n_skels = unique_skels.len();
        let saved = n_codes - n_skels;
        let reduction = saved as f64 / n_codes as f64 * 100.0;
        println!(
            "  {:24} {:>10} {:>10} {:>10} {:>9.1}%",
            label, n_codes, n_skels, saved, reduction
        );
    }

    let total_elapsed = t0.elapsed();
    println!(
        "\nTotal time: {:.1}s",
        total_elapsed.as_secs_f64()
    );
}
