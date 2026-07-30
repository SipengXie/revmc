//! Static grouping statistics — admission guards (g1/g2), collision splits,
//! and unit counts from the shared grouping path. No compilation.

#[path = "../bin_common.rs"]
#[allow(dead_code)]
mod bin_common;

#[path = "../skelot_common.rs"]
#[allow(dead_code)]
mod skelot_common;

use std::path::Path;
use std::time::Instant;

use std::collections::HashMap;

use clap::Parser;
use revmc::emse::{
    group_skeleton_families, skeleton_bytes, strip_emse_suffixes, tail_can_fall_through,
    SkeletonMode,
};

use crate::skelot_common::collect_bytecodes_cached;

#[derive(Parser)]
#[command(name = "grouping_stats")]
struct Args {
    #[arg(long)]
    bench_dir: String,
    #[arg(long, default_value_t = 38004930)]
    start_block: u64,
    #[arg(long, default_value_t = 10000)]
    block_count: u64,
    #[arg(long, default_value = "emse")]
    skeleton_mode: SkeletonMode,
    /// Print the first N g1-rejected groups and classify rejected members by
    /// stripped-suffix content (all-zero vs contains-metadata).
    #[arg(long, default_value_t = 0)]
    dump_g1: usize,
}

fn main() {
    let args = Args::parse();
    let t0 = Instant::now();
    let contracts =
        collect_bytecodes_cached(Path::new(&args.bench_dir), args.start_block, args.block_count);
    eprintln!(
        "Collected {} compilable bytecodes in {:.1}s",
        contracts.len(),
        t0.elapsed().as_secs_f64()
    );

    let t1 = Instant::now();
    let grouping = group_skeleton_families(&contracts, args.skeleton_mode);
    let group_s = t1.elapsed().as_secs_f64();

    let multi = grouping.families.len();
    let singletons = grouping.per_hash.len();
    let member_sum: usize = grouping.families.iter().map(|f| f.members.len()).sum();
    let s = &grouping.stats;
    println!("skeleton_mode: {}", args.skeleton_mode.as_str());
    println!("bytecodes: {}", contracts.len());
    println!("multi_member_families: {multi}");
    println!("family_members_total: {member_sum}");
    println!("per_hash_units_incl_rejected: {singletons}");
    println!("compile_units_total: {}", multi + singletons);
    println!("grouping_seconds: {group_s:.2}");
    println!("g1_rejected_skeletons: {}", s.g1_rejected_skeletons);
    println!("g1_rejected_members: {}", s.g1_rejected_members);
    println!("g2_rejected_members: {}", s.g2_rejected_members);
    println!("collision_groups_split: {}", s.collision_groups_split);

    if args.dump_g1 > 0 {
        let mut groups: HashMap<Vec<u8>, Vec<&revm::primitives::B256>> = HashMap::new();
        for (hash, bytes) in &contracts {
            groups.entry(skeleton_bytes(bytes, args.skeleton_mode)).or_default().push(hash);
        }
        let (mut rej_groups, mut zero_members, mut nonzero_members, mut shown) = (0, 0, 0, 0);
        for members in groups.values().filter(|m| m.len() >= 2) {
            let rep = &contracts[members[0]];
            let code = strip_emse_suffixes(rep);
            if !tail_can_fall_through(code) {
                continue;
            }
            rej_groups += 1;
            let (mut zero, mut nonzero) = (0, 0);
            for h in members {
                let full = &contracts[*h];
                let stripped_len = strip_emse_suffixes(full).len();
                if full[stripped_len..].iter().all(|&b| b == 0) {
                    zero += 1;
                } else {
                    nonzero += 1;
                }
            }
            zero_members += zero;
            nonzero_members += nonzero;
            if shown < args.dump_g1 {
                let tail = &code[code.len().saturating_sub(12)..];
                println!(
                    "g1 group: {} members ({} all-zero suffix / {} with metadata), code tail: {}",
                    members.len(),
                    zero,
                    nonzero,
                    hex::encode(tail)
                );
                shown += 1;
            }
        }
        println!("g1 dump total: {rej_groups} groups");
        println!("g1 members with ALL-ZERO stripped suffix (provably safe, over-rejected): {zero_members}");
        println!("g1 members with metadata in stripped suffix (must reject): {nonzero_members}");
    }
}
