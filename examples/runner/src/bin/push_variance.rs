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

    // ── Variant Depth Analysis ────────────────────────────────────────────
    // For each variant PUSH position: how many distinct values? Is it
    // "one bad apple" (majority value + 1-2 outliers) or truly diverse?
    println!("\n=== Variant Depth Analysis (\"耗子屎\" detection) ===");
    println!("For each variant PUSH: how many distinct values exist across all instances?\n");

    // Per-position stats across ALL groups
    let mut total_variant_positions = 0u64;
    let mut near_invariant_positions = 0u64;  // majority >= 99% of members
    let mut low_diversity_positions = 0u64;   // 2-5 distinct values
    let mut high_diversity_positions = 0u64;  // many distinct values (truly variant)

    // Histogram: distinct value count → how many PUSH positions
    let mut distinct_count_histogram: HashMap<usize, usize> = HashMap::new();

    // Collect detailed per-position data for top groups
    struct VariantPositionStats {
        group_rank: usize,
        group_copies: usize,
        push_idx: usize,
        opcode: u8,
        distinct_values: usize,
        majority_count: usize,     // how many instances have the most common value
        outlier_count: usize,      // members - majority_count
    }
    let mut all_variant_positions: Vec<VariantPositionStats> = Vec::new();

    for (rank, (_skel_hash, members)) in dup_groups.iter().enumerate() {
        let n = members.len();
        let all_pushes: Vec<Vec<PushInfo>> =
            members.iter().map(|(_, b)| extract_pushes(b)).collect();
        let ref_pushes = &all_pushes[0];
        let n_pushes = ref_pushes.len();

        for push_idx in 0..n_pushes {
            let ref_push = &ref_pushes[push_idx];

            // Check if variant
            let all_same = all_pushes.iter().all(|pushes| {
                pushes.get(push_idx).map_or(false, |p| p.value == ref_push.value)
            });
            if all_same {
                continue;
            }

            total_variant_positions += 1;

            // Count distinct values and find majority
            let mut value_counts: HashMap<Vec<u8>, usize> = HashMap::new();
            for pushes in &all_pushes {
                if let Some(p) = pushes.get(push_idx) {
                    *value_counts.entry(p.value.clone()).or_default() += 1;
                }
            }
            let distinct = value_counts.len();
            let majority_count = *value_counts.values().max().unwrap_or(&0);
            let outlier_count = n - majority_count;

            *distinct_count_histogram.entry(distinct).or_default() += 1;

            let majority_pct = majority_count as f64 / n as f64 * 100.0;
            if majority_pct >= 99.0 {
                near_invariant_positions += 1;
            }
            if distinct <= 5 {
                low_diversity_positions += 1;
            } else {
                high_diversity_positions += 1;
            }

            all_variant_positions.push(VariantPositionStats {
                group_rank: rank,
                group_copies: n,
                push_idx,
                opcode: ref_push.opcode,
                distinct_values: distinct,
                majority_count,
                outlier_count,
            });
        }
    }

    // Summary
    println!("Total variant PUSH positions: {total_variant_positions}");
    println!(
        "  Near-invariant (majority >= 99%): {} ({:.1}%) ← \"耗子屎\" scenario",
        near_invariant_positions,
        if total_variant_positions > 0 { near_invariant_positions as f64 / total_variant_positions as f64 * 100.0 } else { 0.0 }
    );
    println!(
        "  Low diversity (2-5 distinct values): {} ({:.1}%)",
        low_diversity_positions,
        if total_variant_positions > 0 { low_diversity_positions as f64 / total_variant_positions as f64 * 100.0 } else { 0.0 }
    );
    println!(
        "  High diversity (>5 distinct values): {} ({:.1}%) ← truly variant",
        high_diversity_positions,
        if total_variant_positions > 0 { high_diversity_positions as f64 / total_variant_positions as f64 * 100.0 } else { 0.0 }
    );

    // Distinct value count histogram
    println!("\n  Distinct value count histogram:");
    println!("  {:>12}  {:>8}  {:>8}", "distinct_vals", "positions", "pct");
    let mut hist_entries: Vec<(usize, usize)> = distinct_count_histogram.into_iter().collect();
    hist_entries.sort_by_key(|(k, _)| *k);
    // Bucket large values
    let mut bucketed: Vec<(String, usize)> = Vec::new();
    let mut large_sum = 0usize;
    for (distinct, count) in &hist_entries {
        if *distinct <= 10 {
            bucketed.push((format!("{}", distinct), *count));
        } else if *distinct <= 50 {
            large_sum += count;
        }
    }
    if large_sum > 0 {
        bucketed.push(("11-50".to_string(), large_sum));
    }
    let mut very_large_sum = 0usize;
    for (distinct, count) in &hist_entries {
        if *distinct > 50 && *distinct <= 200 {
            very_large_sum += count;
        }
    }
    if very_large_sum > 0 {
        bucketed.push(("51-200".to_string(), very_large_sum));
    }
    let mut huge_sum = 0usize;
    for (distinct, count) in &hist_entries {
        if *distinct > 200 {
            huge_sum += count;
        }
    }
    if huge_sum > 0 {
        bucketed.push((">200".to_string(), huge_sum));
    }
    for (label, count) in &bucketed {
        println!(
            "  {:>12}  {:>8}  {:>7.1}%",
            label, count,
            *count as f64 / total_variant_positions as f64 * 100.0
        );
    }

    // Top "near-invariant" positions (耗子屎 examples)
    let mut near_inv: Vec<&VariantPositionStats> = all_variant_positions.iter()
        .filter(|v| {
            let majority_pct = v.majority_count as f64 / v.group_copies as f64 * 100.0;
            majority_pct >= 95.0 && v.group_copies >= 10
        })
        .collect();
    near_inv.sort_by_key(|v| std::cmp::Reverse(v.group_copies));

    if !near_inv.is_empty() {
        println!("\n  Top \"耗子屎\" positions (majority >= 95%, group >= 10 members):");
        println!("  {:>6}  {:>6}  {:>8}  {:>8}  {:>9}  {:>10}  {:>10}",
            "group#", "copies", "push_idx", "opcode", "distinct", "majority", "outliers");
        for (i, v) in near_inv.iter().enumerate().take(30) {
            let push_name = format!("PUSH{}", v.opcode - 0x5f);
            println!(
                "  {:>6}  {:>6}  {:>8}  {:>8}  {:>9}  {:>9} ({:>4.1}%)  {:>10}",
                v.group_rank + 1, v.group_copies, v.push_idx, push_name,
                v.distinct_values, v.majority_count,
                v.majority_count as f64 / v.group_copies as f64 * 100.0,
                v.outlier_count
            );
            if i >= 29 { break; }
        }
    }

    // Top "truly variant" positions
    let mut truly_var: Vec<&VariantPositionStats> = all_variant_positions.iter()
        .filter(|v| v.distinct_values > 5 && v.group_copies >= 10)
        .collect();
    truly_var.sort_by_key(|v| std::cmp::Reverse(v.distinct_values));

    if !truly_var.is_empty() {
        println!("\n  Top \"truly variant\" positions (>5 distinct values, group >= 10 members):");
        println!("  {:>6}  {:>6}  {:>8}  {:>8}  {:>9}  {:>10}  {:>10}",
            "group#", "copies", "push_idx", "opcode", "distinct", "majority", "outliers");
        for (i, v) in truly_var.iter().enumerate().take(30) {
            let push_name = format!("PUSH{}", v.opcode - 0x5f);
            println!(
                "  {:>6}  {:>6}  {:>8}  {:>8}  {:>9}  {:>9} ({:>4.1}%)  {:>10}",
                v.group_rank + 1, v.group_copies, v.push_idx, push_name,
                v.distinct_values, v.majority_count,
                v.majority_count as f64 / v.group_copies as f64 * 100.0,
                v.outlier_count
            );
            if i >= 29 { break; }
        }
    }

    // Per-group summary: how many sub-variants would lazy promotion need?
    println!("\n=== Lazy Promotion Impact Estimate ===");
    println!("If we use majority value as invariant and lazy-promote outliers:\n");

    struct LazyPromotionStats {
        group_rank: usize,
        copies: usize,
        variant_positions: usize,
        near_invariant_positions: usize,   // could be treated as invariant in primary
        truly_variant_positions: usize,     // must stay in data table
        max_outliers: usize,               // max outlier count across all near-invariant positions
        total_outlier_bytecodes: usize,    // unique bytecodes that are outliers on any position
    }
    let mut lazy_stats: Vec<LazyPromotionStats> = Vec::new();

    // Regroup variant positions by group
    let mut group_variant_positions: HashMap<usize, Vec<&VariantPositionStats>> = HashMap::new();
    for v in &all_variant_positions {
        group_variant_positions.entry(v.group_rank).or_default().push(v);
    }

    for (rank, (_skel_hash, members)) in dup_groups.iter().enumerate() {
        let positions = match group_variant_positions.get(&rank) {
            Some(p) => p,
            None => continue,
        };
        let n = members.len();
        let mut near_inv_count = 0usize;
        let mut truly_var_count = 0usize;
        let mut max_outliers = 0usize;

        // Track which bytecodes are outliers on any position
        let all_pushes: Vec<Vec<PushInfo>> =
            members.iter().map(|(_, b)| extract_pushes(b)).collect();

        let mut outlier_bytecodes = std::collections::HashSet::new();

        for v in positions {
            let majority_pct = v.majority_count as f64 / n as f64 * 100.0;
            if majority_pct >= 90.0 {
                near_inv_count += 1;
                if v.outlier_count > max_outliers {
                    max_outliers = v.outlier_count;
                }
                // Find which bytecodes are outliers at this position
                let ref_pushes_for_pos = &all_pushes[0];
                if let Some(ref_push) = ref_pushes_for_pos.get(v.push_idx) {
                    // Find majority value
                    let mut value_counts: HashMap<Vec<u8>, usize> = HashMap::new();
                    for pushes in &all_pushes {
                        if let Some(p) = pushes.get(v.push_idx) {
                            *value_counts.entry(p.value.clone()).or_default() += 1;
                        }
                    }
                    let majority_value = value_counts.iter().max_by_key(|(_, c)| **c).map(|(v, _)| v.clone());
                    if let Some(maj_val) = majority_value {
                        for (idx, pushes) in all_pushes.iter().enumerate() {
                            if let Some(p) = pushes.get(v.push_idx) {
                                if p.value != maj_val {
                                    outlier_bytecodes.insert(idx);
                                }
                            }
                        }
                    }
                }
            } else {
                truly_var_count += 1;
            }
        }

        if near_inv_count > 0 || truly_var_count > 0 {
            lazy_stats.push(LazyPromotionStats {
                group_rank: rank,
                copies: n,
                variant_positions: positions.len(),
                near_invariant_positions: near_inv_count,
                truly_variant_positions: truly_var_count,
                max_outliers,
                total_outlier_bytecodes: outlier_bytecodes.len(),
            });
        }
    }

    lazy_stats.sort_by_key(|s| std::cmp::Reverse(s.copies));

    println!("  {:>6}  {:>6}  {:>8}  {:>10}  {:>10}  {:>12}  {:>14}",
        "group#", "copies", "var_pos", "near_inv", "truly_var", "max_outliers", "outlier_bcs");
    for s in lazy_stats.iter().take(30) {
        println!(
            "  {:>6}  {:>6}  {:>8}  {:>10}  {:>10}  {:>12}  {:>13} ({:.1}%)",
            s.group_rank + 1, s.copies, s.variant_positions,
            s.near_invariant_positions, s.truly_variant_positions,
            s.max_outliers, s.total_outlier_bytecodes,
            s.total_outlier_bytecodes as f64 / s.copies as f64 * 100.0
        );
    }

    // Grand total
    let total_bytecodes_in_groups: usize = lazy_stats.iter().map(|s| s.copies).sum();
    let total_outlier_bcs: usize = lazy_stats.iter().map(|s| s.total_outlier_bytecodes).sum();
    let total_could_promote: usize = lazy_stats.iter().map(|s| s.near_invariant_positions).sum();
    let total_truly_var: usize = lazy_stats.iter().map(|s| s.truly_variant_positions).sum();
    println!("\n  Grand total:");
    println!("    Bytecodes in variant groups: {total_bytecodes_in_groups}");
    println!("    Variant positions that are near-invariant (promotable): {total_could_promote}");
    println!("    Variant positions that are truly variant: {total_truly_var}");
    println!("    Total outlier bytecodes needing sub-variants: {total_outlier_bcs} ({:.1}%)",
        if total_bytecodes_in_groups > 0 { total_outlier_bcs as f64 / total_bytecodes_in_groups as f64 * 100.0 } else { 0.0 });
}
