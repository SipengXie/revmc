//! Diagnose skeleton families whose Auto variance classification collides
//! with static-jump folding: a PUSH that differs across members (Variant)
//! but feeds an adjacent JUMP/JUMPI (SKIP_LOGIC). Compiling such a family
//! panics in `apply_variance`; this reports how many families/members are
//! affected without compiling anything.

#[path = "../bin_common.rs"]
#[allow(dead_code)]
mod bin_common;

#[path = "../skelot_common.rs"]
#[allow(dead_code)]
mod skelot_common;

use std::path::Path;

use clap::Parser;
use revmc::emse::{bytecode_view, group_skeleton_families, SkeletonMode};
use revmc::skeleton::{analyze_skeleton_group_with_mode, ClassifyMode, PushClassification};

use crate::skelot_common::collect_bytecodes_cached;

#[derive(Parser)]
#[command(name = "skip_logic_conflicts")]
struct Args {
    #[arg(long)]
    bench_dir: String,
    #[arg(long, default_value_t = 38004930)]
    start_block: u64,
    #[arg(long, default_value_t = 10000)]
    block_count: u64,
    #[arg(long, default_value = "emse")]
    skeleton_mode: SkeletonMode,
}

fn main() {
    let args = Args::parse();
    let mut contracts =
        collect_bytecodes_cached(Path::new(&args.bench_dir), args.start_block, args.block_count);
    contracts.retain(|_, bytes| !skelot_common::should_skip_bytes(bytes));
    let grouping = group_skeleton_families(&contracts, args.skeleton_mode);

    let mut conflict_families = 0usize;
    let mut conflict_members = 0usize;
    let mut total_conflict_pushes = 0usize;
    for family in &grouping.families {
        let views: Vec<&[u8]> = family
            .members
            .iter()
            .map(|h| bytecode_view(&contracts[h], args.skeleton_mode))
            .collect();
        let variance = analyze_skeleton_group_with_mode(&views, ClassifyMode::Auto);
        let skip = revmc::skeleton::skip_logic_push_mask(views[0]);
        let conflicts: Vec<usize> = variance
            .pushes
            .iter()
            .enumerate()
            .filter(|(i, p)| {
                matches!(p, PushClassification::Variant { .. }) && skip.get(*i).copied().unwrap_or(false)
            })
            .map(|(i, _)| i)
            .collect();
        if !conflicts.is_empty() {
            conflict_families += 1;
            conflict_members += family.members.len();
            total_conflict_pushes += conflicts.len();
            println!(
                "family {:016x}: {} members, {} conflicting PUSHes (ordinals {:?})",
                family.skel_hash,
                family.members.len(),
                conflicts.len(),
                &conflicts[..conflicts.len().min(8)]
            );
        }
    }
    println!("---");
    println!("families total: {}", grouping.families.len());
    println!("conflict families: {conflict_families}");
    println!("conflict members: {conflict_members}");
    println!("conflict pushes total: {total_conflict_pushes}");
}
