//! Analyze which PUSH immediates actually vary across instances of the same skeleton.
//!
//! For each skeleton group with 2+ members, compare every PUSH position:
//!   - "invariant" PUSHes: same value across ALL instances (selectors, internal constants)
//!   - "variant" PUSHes: different values (addresses, deployment-specific config)
//!
//! This determines which PUSHes can remain as compile-time constants
//! and which must be parameterized in a skeleton-aware compilation.

use std::collections::HashMap;
use std::path::Path;

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

/// A single PUSH instruction's position and value in a bytecode.
#[derive(Clone)]
struct PushInfo {
    /// Index of this PUSH in the opcode stream (0-based, counting only opcodes not imm bytes)
    opcode_index: usize,
    /// The PUSH opcode (0x60..0x7f)
    opcode: u8,
    /// The immediate value bytes (big-endian)
    value: Vec<u8>,
}

/// Extract all PUSH instructions with their positions and values.
fn extract_pushes(bytes: &[u8]) -> Vec<PushInfo> {
    let mut pushes = Vec::new();
    let mut i = 0;
    let mut opcode_index = 0;
    while i < bytes.len() {
        let op = bytes[i];
        i += 1;
        if op >= 0x60 && op <= 0x7f {
            let n = (op - 0x5f) as usize;
            let value = if i + n <= bytes.len() {
                bytes[i..i + n].to_vec()
            } else {
                // Truncated PUSH at end of bytecode
                let available = bytes.len().saturating_sub(i);
                bytes[i..i + available].to_vec()
            };
            pushes.push(PushInfo {
                opcode_index,
                opcode: op,
                value,
            });
            i += n;
        }
        opcode_index += 1;
    }
    pushes
}

fn hash_skeleton(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut skeleton = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let op = bytes[i];
        skeleton.push(op);
        i += 1;
        if op >= 0x60 && op <= 0x7f {
            i += (op - 0x5f) as usize;
        }
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    skeleton.hash(&mut hasher);
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

    eprintln!("Scanning blocks {start}..{end} ({count} blocks)");

    // Phase 1: collect unique bytecodes
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
        if scanned % 2000 == 0 {
            eprintln!("  {scanned} blocks, {} unique codes", all_codes.len());
        }
    }
    eprintln!("Scanned {scanned} blocks, {} unique bytecodes", all_codes.len());

    // Phase 2: group by skeleton hash
    let mut skeleton_groups: HashMap<u64, Vec<(B256, Vec<u8>)>> = HashMap::new();
    for (hash, bytes) in &all_codes {
        if bytes.is_empty() {
            continue;
        }
        let skel_hash = hash_skeleton(bytes);
        skeleton_groups
            .entry(skel_hash)
            .or_default()
            .push((*hash, bytes.clone()));
    }

    // Filter to groups with 2+ members, sort by size desc
    let mut dup_groups: Vec<(u64, Vec<(B256, Vec<u8>)>)> = skeleton_groups
        .into_iter()
        .filter(|(_, members)| members.len() >= 2)
        .collect();
    dup_groups.sort_by_key(|(_, m)| std::cmp::Reverse(m.len()));

    println!("=== PUSH Variance Analysis ===");
    println!("Skeleton groups with 2+ members: {}\n", dup_groups.len());

    // Global stats
    let mut total_push_positions = 0u64;
    let mut total_invariant = 0u64;
    let mut total_variant = 0u64;
    let mut total_invariant_bytes = 0u64;
    let mut total_variant_bytes = 0u64;

    // Per-group stats for distribution analysis
    struct GroupStats {
        copies: usize,
        bytecode_len: usize,
        n_pushes: usize,
        invariant_count: usize,
        variant_count: usize,
        invariant_bytes: usize,
        variant_bytes: usize,
    }
    let mut all_group_stats: Vec<GroupStats> = Vec::new();

    // Analyze top groups in detail
    let detail_count = 20;
    for (rank, (skel_hash, members)) in dup_groups.iter().enumerate() {
        let n = members.len();
        let bytecode_len = members[0].1.len();

        // Extract pushes for all members
        let all_pushes: Vec<Vec<PushInfo>> =
            members.iter().map(|(_, b)| extract_pushes(b)).collect();

        // Use first member as reference
        let ref_pushes = &all_pushes[0];
        let n_pushes = ref_pushes.len();

        // Compare each PUSH position across all members
        let mut invariant_count = 0usize;
        let mut variant_count = 0usize;
        let mut invariant_bytes_sum = 0usize;
        let mut variant_bytes_sum = 0usize;
        let mut variant_details: Vec<(usize, u8, usize)> = Vec::new(); // (opcode_idx, opcode, imm_len)

        for push_idx in 0..n_pushes {
            let ref_push = &ref_pushes[push_idx];
            let imm_len = ref_push.value.len();

            // Check if all members have the same value at this position
            let all_same = all_pushes.iter().all(|pushes| {
                pushes.get(push_idx).map_or(false, |p| p.value == ref_push.value)
            });

            if all_same {
                invariant_count += 1;
                invariant_bytes_sum += imm_len;
            } else {
                variant_count += 1;
                variant_bytes_sum += imm_len;
                variant_details.push((ref_push.opcode_index, ref_push.opcode, imm_len));
            }
        }

        total_push_positions += n_pushes as u64;
        total_invariant += invariant_count as u64;
        total_variant += variant_count as u64;
        total_invariant_bytes += invariant_bytes_sum as u64;
        total_variant_bytes += variant_bytes_sum as u64;

        all_group_stats.push(GroupStats {
            copies: n,
            bytecode_len,
            n_pushes,
            invariant_count,
            variant_count,
            invariant_bytes: invariant_bytes_sum,
            variant_bytes: variant_bytes_sum,
        });

        if rank < detail_count {
            let variant_pct = if n_pushes > 0 {
                variant_count as f64 / n_pushes as f64 * 100.0
            } else {
                0.0
            };
            println!(
                "━━━ #{} | skel {:016x} | {} copies | {}B bytecode ━━━",
                rank + 1,
                skel_hash,
                n,
                bytecode_len
            );
            println!(
                "  PUSH count: {}  |  invariant: {} ({} bytes)  |  variant: {} ({} bytes, {:.1}%)",
                n_pushes, invariant_count, invariant_bytes_sum,
                variant_count, variant_bytes_sum, variant_pct
            );
            if !variant_details.is_empty() {
                print!("  Variant PUSHes: ");
                for (i, (opcode_idx, opcode, imm_len)) in variant_details.iter().enumerate() {
                    if i > 0 { print!(", "); }
                    let push_name = format!("PUSH{}", opcode - 0x5f);
                    print!("#{opcode_idx}({push_name},{imm_len}B)");
                    if i >= 14 {
                        let remaining = variant_details.len() - i - 1;
                        if remaining > 0 {
                            print!(", ...+{remaining} more");
                        }
                        break;
                    }
                }
                println!();

                // Show data table size
                println!(
                    "  Data table: {} bytes/instance × {} instances = {} bytes total",
                    variant_bytes_sum,
                    n,
                    variant_bytes_sum * n
                );
                // Show what fraction of the original PUSH data varies
                let total_imm = invariant_bytes_sum + variant_bytes_sum;
                if total_imm > 0 {
                    println!(
                        "  Only {:.1}% of immediate bytes vary ({}/{}B)",
                        variant_bytes_sum as f64 / total_imm as f64 * 100.0,
                        variant_bytes_sum,
                        total_imm
                    );
                }
            } else {
                // All PUSHes are invariant - these differ only in non-PUSH bytes?
                // This shouldn't happen for skeleton-identical contracts
                println!("  ALL PUSHes invariant (bytecodes differ only in metadata?)");
            }
            println!();
        }
    }

    // Global summary
    println!("=== Global Summary (all {} skeleton groups with 2+ members) ===", dup_groups.len());
    println!("  Total PUSH positions analyzed: {total_push_positions}");
    println!(
        "  Invariant: {total_invariant} ({total_invariant_bytes} bytes) — can remain LLVM constants"
    );
    println!(
        "  Variant:   {total_variant} ({total_variant_bytes} bytes) — need parameterization"
    );
    if total_push_positions > 0 {
        println!(
            "  Variant fraction: {:.2}% of positions, {:.2}% of bytes",
            total_variant as f64 / total_push_positions as f64 * 100.0,
            total_variant_bytes as f64 / (total_invariant_bytes + total_variant_bytes) as f64 * 100.0
        );
    }

    // Estimate compilation savings
    let total_dup_bytecodes: usize = dup_groups.iter().map(|(_, m)| m.len()).sum();
    let total_dup_skeletons = dup_groups.len();
    println!(
        "\n  Compilation: {} bytecodes → {} skeleton compilations (each with full constant folding on invariant PUSHes)",
        total_dup_bytecodes, total_dup_skeletons
    );

    // ── Full distribution analysis ──────────────────────────────────────────
    println!("\n=== Variant PUSH Distribution (ALL {} groups) ===", all_group_stats.len());

    // Histogram by variant percentage
    let buckets: &[(f64, f64, &str)] = &[
        (0.0, 0.0, "0% (all invariant)"),
        (0.0, 1.0, "0-1% variant"),
        (1.0, 2.0, "1-2% variant"),
        (2.0, 5.0, "2-5% variant"),
        (5.0, 10.0, "5-10% variant"),
        (10.0, 25.0, "10-25% variant"),
        (25.0, 50.0, "25-50% variant"),
        (50.0, 100.01, "50-100% variant"),
    ];

    println!("\n  Variant % histogram (by # of PUSH positions):");
    println!("  {:>20}  {:>6}  {:>8}  {:>10}", "bucket", "groups", "bytecodes", "avg_copies");
    for &(lo, hi, label) in buckets {
        let matching: Vec<&GroupStats> = all_group_stats.iter().filter(|g| {
            if g.n_pushes == 0 { return lo == 0.0 && hi == 0.0; }
            let pct = g.variant_count as f64 / g.n_pushes as f64 * 100.0;
            if lo == 0.0 && hi == 0.0 {
                pct == 0.0
            } else if lo == 0.0 {
                pct > 0.0 && pct <= hi
            } else {
                pct > lo && pct <= hi
            }
        }).collect();
        let n_groups = matching.len();
        let n_bytecodes: usize = matching.iter().map(|g| g.copies).sum();
        let avg_copies = if n_groups > 0 { n_bytecodes as f64 / n_groups as f64 } else { 0.0 };
        if n_groups > 0 {
            println!("  {:>20}  {:>6}  {:>8}  {:>10.1}", label, n_groups, n_bytecodes, avg_copies);
        }
    }

    // Bytecode size vs variant %
    println!("\n  Variant % by bytecode size:");
    println!("  {:>20}  {:>6}  {:>10}  {:>12}  {:>12}  {:>10}",
        "size bucket", "groups", "bytecodes", "avg_var_push", "avg_var_bytes", "avg_var%");
    let size_buckets: &[(usize, usize, &str)] = &[
        (0, 100, "tiny (<100B)"),
        (100, 500, "small (100-500B)"),
        (500, 5000, "medium (500B-5KB)"),
        (5000, 15000, "large (5-15KB)"),
        (15000, usize::MAX, "huge (>15KB)"),
    ];
    for &(lo, hi, label) in size_buckets {
        let matching: Vec<&GroupStats> = all_group_stats.iter()
            .filter(|g| g.bytecode_len >= lo && g.bytecode_len < hi)
            .collect();
        let n_groups = matching.len();
        if n_groups == 0 { continue; }
        let n_bytecodes: usize = matching.iter().map(|g| g.copies).sum();
        let avg_variant_pushes: f64 = matching.iter().map(|g| g.variant_count as f64).sum::<f64>() / n_groups as f64;
        let avg_variant_bytes: f64 = matching.iter().map(|g| g.variant_bytes as f64).sum::<f64>() / n_groups as f64;
        let avg_variant_pct: f64 = matching.iter()
            .filter(|g| g.n_pushes > 0)
            .map(|g| g.variant_count as f64 / g.n_pushes as f64 * 100.0)
            .sum::<f64>() / matching.iter().filter(|g| g.n_pushes > 0).count().max(1) as f64;
        println!("  {:>20}  {:>6}  {:>10}  {:>12.1}  {:>11.0}B  {:>9.1}%",
            label, n_groups, n_bytecodes, avg_variant_pushes, avg_variant_bytes, avg_variant_pct);
    }

    // Full table: every group, sorted by variant%
    println!("\n  Full table (all {} groups, sorted by variant%):", all_group_stats.len());
    println!("  {:>4}  {:>6}  {:>7}  {:>6}  {:>6}  {:>6}  {:>8}  {:>7}",
        "rank", "copies", "bcLen", "pushes", "invar", "var", "var%", "varB");

    // Sort by variant percentage descending
    let mut indexed: Vec<(usize, &GroupStats)> = all_group_stats.iter().enumerate().collect();
    indexed.sort_by(|a, b| {
        let pct_a = if a.1.n_pushes > 0 { a.1.variant_count as f64 / a.1.n_pushes as f64 } else { 0.0 };
        let pct_b = if b.1.n_pushes > 0 { b.1.variant_count as f64 / b.1.n_pushes as f64 } else { 0.0 };
        pct_b.partial_cmp(&pct_a).unwrap()
    });

    for (table_rank, (_orig_idx, g)) in indexed.iter().enumerate() {
        let var_pct = if g.n_pushes > 0 {
            g.variant_count as f64 / g.n_pushes as f64 * 100.0
        } else {
            0.0
        };
        println!("  {:>4}  {:>6}  {:>6}B  {:>6}  {:>6}  {:>6}  {:>7.1}%  {:>6}B",
            table_rank + 1, g.copies, g.bytecode_len, g.n_pushes,
            g.invariant_count, g.variant_count, var_pct, g.variant_bytes);
    }
}
