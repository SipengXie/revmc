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
            // Forensics: who are the members, where exactly do they differ,
            // and what are the diverging jump targets?
            for h in &family.members {
                let full = &contracts[h];
                let stripped = revmc::emse::strip_emse_suffixes(full);
                println!(
                    "  member 0x{} raw_len={} stripped_len={}",
                    hex::encode(h),
                    full.len(),
                    stripped.len()
                );
            }
            let rep_len =
                revmc::emse::strip_emse_suffixes(&contracts[&family.members[0]]).len();
            let stripped_views: Vec<&[u8]> = family
                .members
                .iter()
                .map(|h| &revmc::emse::strip_emse_suffixes(&contracts[h])[..rep_len])
                .collect();
            let mut diff_offsets: Vec<usize> = Vec::new();
            for i in 0..rep_len {
                if stripped_views.iter().any(|v| v[i] != stripped_views[0][i]) {
                    diff_offsets.push(i);
                }
            }
            println!(
                "  in-region diff bytes: {} at offsets {:?}{}",
                diff_offsets.len(),
                &diff_offsets[..diff_offsets.len().min(30)],
                if diff_offsets.len() > 30 { " ..." } else { "" }
            );
            let imms: Vec<Vec<(usize, &[u8])>> = stripped_views
                .iter()
                .map(|v| revmc::skeleton::extract_push_immediates(v))
                .collect();
            for &ord in conflicts.iter() {
                let vals: Vec<String> =
                    imms.iter().map(|m| hex::encode(m[ord].1)).collect();
                println!("    conflict push #{ord}: member values {vals:?}");
            }
        }
    }
    println!("---");
    println!("families total: {}", grouping.families.len());
    println!("conflict families: {conflict_families}");
    println!("conflict members: {conflict_members}");
    println!("conflict pushes total: {total_conflict_pushes}");
}
