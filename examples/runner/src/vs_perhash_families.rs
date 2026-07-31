//! Phase-2 family loading for the skeleton-vs-per-hash comparison: either
//! deserialize a pre-built registry (fast path) or scan the corpus, run the
//! shared verified grouping, and persist a fresh registry (slow path).
//! Split out of `skeleton_vs_perhash_bench.rs`. Variance must match what was
//! used at compile time — a different member set produces wrong data-table
//! slot assignments and silent correctness bugs.

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use revm::primitives::B256;
use revmc::emse::{bytecode_view, group_skeleton_families, SkeletonMode};
use revmc::skeleton::{analyze_skeleton_group_with_mode, build_data_table, ClassifyMode};

use crate::bin_common::BinLoader;

#[derive(serde::Serialize, serde::Deserialize)]
pub struct RegistryEntry {
    pub skel_hash: u64,
    pub member_hashes: Vec<[u8; 32]>,
    pub variance_pushes: Vec<i32>, // -1 = Invariant, >= 0 = Variant(table_index)
    pub num_variant: u32,
    #[serde(default)]
    pub member_data_tables: Vec<Vec<u8>>, // pre-computed data tables, one per member
    /// `true` iff this registry was written after static-jump-conflicting
    /// families were filtered out at build time. Old registries (default
    /// false) may still contain such families and must be rebuilt.
    #[serde(default)]
    pub static_jump_checked: bool,
}

pub struct AnalyzedGroup {
    pub skel_hash: u64,
    pub members: Vec<(B256, Vec<u8>)>,
    pub variance: revmc::skeleton::SkeletonVariance,
    pub cached_data_tables: Option<Vec<(B256, Vec<u8>)>>,
}

fn variance_from_registry(entry: &RegistryEntry) -> revmc::skeleton::SkeletonVariance {
    use revmc::skeleton::PushClassification;
    let pushes = entry
        .variance_pushes
        .iter()
        .map(|&v| {
            if v < 0 {
                PushClassification::Invariant
            } else {
                PushClassification::Variant { table_index: v as u32 }
            }
        })
        .collect();
    revmc::skeleton::SkeletonVariance { pushes, num_variant: entry.num_variant }
}

#[allow(clippy::too_many_arguments)]
pub fn load_or_build_families(
    registry: Option<&str>,
    out: Option<&str>,
    bench_dir: &Path,
    start_block: u64,
    block_count: u64,
    skeleton_mode: SkeletonMode,
    classify_mode: ClassifyMode,
    t1: Instant,
) -> Vec<AnalyzedGroup> {
    let analyzed: Vec<AnalyzedGroup>;

    if let Some(registry_path) = registry {
        // Fast path: load pre-built registry with cached data tables
        println!("\n--- Phase 2: Loading skeleton registry from {} ---", registry_path);
        let data = std::fs::read(registry_path).expect("read registry");
        let entries: Vec<RegistryEntry> =
            bincode::deserialize(&data).expect("deserialize registry");

        // Refuse registries written before the static-jump degradation
        // filter existed: they may still contain families whose members
        // jump to different static targets, which the compile driver now
        // degrades to per-hash — loading one here would silently shrink or
        // skew the skeleton-dispatched set relative to the compile run.
        if entries.iter().any(|e| !e.static_jump_checked) {
            panic!(
                "registry {} predates the static-jump degradation filter; \
                 delete it and re-run without --registry to rebuild",
                registry_path
            );
        }

        let has_cached_tables = entries.first().map_or(false, |e| !e.member_data_tables.is_empty());

        if has_cached_tables {
            // Ultra-fast path: data tables pre-computed, no bytecode scan needed
            println!("  Using cached data tables (no bytecode scan)");
            // We don't need AnalyzedGroup.members bytecodes — only for build_data_table
            // which is already cached. Store empty bytecodes as placeholder.
            analyzed = entries
                .into_iter()
                .filter(|entry| {
                    entry.member_hashes.len() >= 2
                        && entry.member_data_tables.len() == entry.member_hashes.len()
                })
                .map(|entry| {
                    let variance = variance_from_registry(&entry);
                    let members: Vec<(B256, Vec<u8>)> = entry
                        .member_hashes
                        .iter()
                        .map(|raw| (B256::from(*raw), Vec::new()))
                        .collect();
                    AnalyzedGroup {
                        skel_hash: entry.skel_hash,
                        members,
                        variance,
                        cached_data_tables: Some(
                            entry
                                .member_hashes
                                .iter()
                                .zip(entry.member_data_tables.into_iter())
                                .map(|(raw, dt)| (B256::from(*raw), dt))
                                .collect(),
                        ),
                    }
                })
                .collect();
        } else {
            // Registry without cached tables — fall back to bytecode scan
            println!("  Registry lacks cached data tables, scanning bytecodes...");
            let mut all_codes: HashMap<B256, Vec<u8>> = HashMap::new();
            let mut blocks_loaded = 0usize;
            for bn in start_block..start_block + block_count {
                if let Ok(loader) = BinLoader::new(bench_dir, bn) {
                    for (h, bc) in loader.code_values() {
                        if !bc.is_empty() {
                            all_codes
                                .entry(*h)
                                .or_insert_with(|| bc.original_byte_slice().to_vec());
                        }
                    }
                    blocks_loaded += 1;
                    if blocks_loaded % 1000 == 0 {
                        eprint!("  [{blocks_loaded}] {} unique bytecodes\r", all_codes.len());
                    }
                }
            }
            println!("  Scanned {blocks_loaded} blocks, {} unique bytecodes", all_codes.len());

            analyzed = entries
                .into_iter()
                .filter_map(|entry| {
                    let variance = variance_from_registry(&entry);
                    let members: Vec<(B256, Vec<u8>)> = entry
                        .member_hashes
                        .iter()
                        .filter_map(|raw| {
                            let hash = B256::from(*raw);
                            all_codes.get(&hash).map(|bytes| (hash, bytes.clone()))
                        })
                        .collect();
                    if members.len() >= 2 {
                        Some(AnalyzedGroup {
                            skel_hash: entry.skel_hash,
                            members,
                            variance,
                            cached_data_tables: None,
                        })
                    } else {
                        None
                    }
                })
                .collect();
        }

        println!(
            "  {} skeleton families loaded from registry in {:.2}s",
            analyzed.len(),
            t1.elapsed().as_secs_f64()
        );
    } else {
        // Slow path: full scan + analyze
        println!(
            "\n--- Phase 2: Reconstructing skeleton families (full {}-block scan) ---",
            block_count
        );

        let mut all_codes: HashMap<B256, Vec<u8>> = HashMap::new();
        let mut blocks_loaded = 0usize;
        for bn in start_block..start_block + block_count {
            if let Ok(loader) = BinLoader::new(bench_dir, bn) {
                for (h, bc) in loader.code_values() {
                    if !bc.is_empty() {
                        all_codes.entry(*h).or_insert_with(|| bc.original_byte_slice().to_vec());
                    }
                }
                blocks_loaded += 1;
                if blocks_loaded % 1000 == 0 {
                    eprint!("  [{blocks_loaded}] {} unique bytecodes\r", all_codes.len());
                }
            }
        }
        println!("  Scanned {blocks_loaded} blocks, {} unique bytecodes", all_codes.len());

        all_codes.retain(|_, bytes| !crate::skelot_common::should_skip_bytes(bytes));

        // Shared grouping: in-bucket skeleton-byte verification plus (under
        // EMSE) the g1/g2 admission guards. Must match the compile driver so
        // guard-rejected members never get skeleton-dispatched here.
        let grouping = group_skeleton_families(&all_codes, skeleton_mode);
        println!(
            "  admission ({}): {} families; g1 rejected {} skeletons ({} members), g2 rejected {} members, {} collision groups split",
            skeleton_mode.as_str(),
            grouping.families.len(),
            grouping.stats.g1_rejected_skeletons,
            grouping.stats.g1_rejected_members,
            grouping.stats.g2_rejected_members,
            grouping.stats.collision_groups_split
        );

        analyzed = grouping
            .families
            .into_iter()
            .filter_map(|family| {
                let views: Vec<&[u8]> = family
                    .members
                    .iter()
                    .map(|h| bytecode_view(&all_codes[h], skeleton_mode))
                    .collect();
                let variance = analyze_skeleton_group_with_mode(&views, classify_mode);
                // Mirror the compile driver: families whose members jump to
                // different static targets are degraded to per-hash and never
                // skeleton-dispatched, so they drop out of the comparison.
                if revmc::skeleton::variance_conflicts_static_jumps(views[0], &variance) {
                    println!(
                        "  skel {:016x}: {} members degraded (variant PUSH feeds a static jump), excluded",
                        family.skel_hash,
                        family.members.len()
                    );
                    return None;
                }
                let members: Vec<(B256, Vec<u8>)> =
                    family.members.into_iter().map(|h| (h, all_codes[&h].clone())).collect();
                Some(AnalyzedGroup { skel_hash: family.skel_hash, members, variance, cached_data_tables: None })
            })
            .collect();

        println!(
            "  {} skeleton families found in {:.2}s",
            analyzed.len(),
            t1.elapsed().as_secs_f64()
        );

        // Save registry for fast reuse (with pre-computed data tables).
        // Both boundary and classify mode get their own filename: member
        // sets, variance, and data tables are specific to the
        // (skeleton_mode, classify_mode) combination and must never cross.
        let registry_file_name = match (skeleton_mode, classify_mode) {
            (SkeletonMode::Current, ClassifyMode::Auto) => "skeleton_registry_full.bin",
            (SkeletonMode::Emse, ClassifyMode::Auto) => "skeleton_registry_full_emse.bin",
            (SkeletonMode::Current, ClassifyMode::ForceAllVariant) => {
                "skeleton_registry_full_forced.bin"
            }
            (SkeletonMode::Emse, ClassifyMode::ForceAllVariant) => {
                "skeleton_registry_full_emse_forced.bin"
            }
        };
        let registry_save_path = out
            .map(|p| std::path::Path::new(p).with_file_name(registry_file_name))
            .unwrap_or_else(|| std::path::PathBuf::from(registry_file_name));
        let entries: Vec<RegistryEntry> = analyzed
            .iter()
            .map(|g| {
                use revmc::skeleton::PushClassification;
                RegistryEntry {
                    skel_hash: g.skel_hash,
                    member_hashes: g.members.iter().map(|(h, _)| h.0).collect(),
                    variance_pushes: g
                        .variance
                        .pushes
                        .iter()
                        .map(|p| match p {
                            PushClassification::Invariant => -1i32,
                            PushClassification::Variant { table_index } => *table_index as i32,
                        })
                        .collect(),
                    num_variant: g.variance.num_variant,
                    member_data_tables: g
                        .members
                        .iter()
                        .map(|(_, bytes)| {
                            build_data_table(bytecode_view(bytes, skeleton_mode), &g.variance)
                                .data
                        })
                        .collect(),
                    static_jump_checked: true,
                }
            })
            .collect();
        let data = bincode::serialize(&entries).expect("serialize registry");
        std::fs::write(&registry_save_path, &data).expect("write registry");
        println!(
            "  Registry saved to {} ({:.1} MB) with cached data tables",
            registry_save_path.display(),
            data.len() as f64 / 1048576.0
        );
    }
    analyzed
}
