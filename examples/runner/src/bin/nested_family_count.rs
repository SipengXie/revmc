//! Untimed interpreter replay that counts nested same-family call frames.
//!
//! Answers reviewer C1: how often does a frame execute while an ancestor
//! frame (and separately: its direct parent) belongs to the same EMSE
//! skeleton family — i.e. the same compiled artifact would sit twice on the
//! call stack with two different data tables.
//!
//! Deliberately standalone and untimed: the timed replay paths are never
//! touched, so the count cannot perturb any timing measurement.

#[path = "../bin_common.rs"]
#[allow(dead_code)]
mod bin_common;

#[path = "../skelot_common.rs"]
#[allow(dead_code)]
mod skelot_common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;
use op_revm::OpHaltReason;
use revm::handler::{EvmTr, FrameResult, Handler, ItemOrResult};
use revm::primitives::B256;
use revmc::emse::{group_skeleton_families, SkeletonMode};
use serde::Serialize;

use bin_common::{build_op_tx, make_evm, BenchError, BenchEvm, BinLoader};
use skelot_common::collect_bytecodes_cached;

#[derive(Parser)]
#[command(
    name = "nested_family_count",
    about = "Count nested same-family call frames over an untimed interpreter replay"
)]
struct Args {
    #[arg(long, default_value = "/home/ubuntu/sipeng/bench_data_base")]
    bench_dir: String,
    #[arg(long, default_value_t = 38004930)]
    start_block: u64,
    #[arg(long, default_value_t = 10000)]
    block_count: u64,
    #[arg(long, default_value = "emse")]
    skeleton_mode: SkeletonMode,
    #[arg(long, default_value_t = 16)]
    threads: usize,
    #[arg(long)]
    out: Option<String>,
}

#[derive(Default, Clone)]
struct Counts {
    blocks_replayed: u64,
    txs_replayed: u64,
    total_frames: u64,
    family_member_frames: u64,
    same_family_ancestor_descendant: u64,
    same_family_parent_child: u64,
    /// skel_hash -> (ancestor-descendant hits, direct parent-child hits)
    per_family: HashMap<u64, (u64, u64)>,
}

impl Counts {
    fn merge(&mut self, other: Counts) {
        self.blocks_replayed += other.blocks_replayed;
        self.txs_replayed += other.txs_replayed;
        self.total_frames += other.total_frames;
        self.family_member_frames += other.family_member_frames;
        self.same_family_ancestor_descendant += other.same_family_ancestor_descendant;
        self.same_family_parent_child += other.same_family_parent_child;
        for (fam, (ad, pc)) in other.per_family {
            let entry = self.per_family.entry(fam).or_insert((0, 0));
            entry.0 += ad;
            entry.1 += pc;
        }
    }
}

/// Interpreter-only handler that mirrors the standard exec loop while
/// tracking the family of every live frame on the call stack.
struct NestedCounter {
    family_of: Arc<HashMap<B256, u64>>,
    stack: Vec<Option<u64>>,
    counts: Counts,
}

impl NestedCounter {
    fn new(family_of: Arc<HashMap<B256, u64>>) -> Self {
        Self { family_of, stack: Vec::new(), counts: Counts::default() }
    }

    fn push_frame(&mut self, evm: &mut BenchEvm) {
        let frame = evm.0.frame_stack.get();
        let hash = frame.interpreter.bytecode.get_or_calculate_hash();
        let fam = self.family_of.get(&hash).copied();
        self.counts.total_frames += 1;
        if let Some(f) = fam {
            self.counts.family_member_frames += 1;
            let parent_same = self.stack.last().copied().flatten() == Some(f);
            let ancestor_same = self.stack.iter().any(|s| *s == Some(f));
            if ancestor_same {
                self.counts.same_family_ancestor_descendant += 1;
                self.counts.per_family.entry(f).or_insert((0, 0)).0 += 1;
            }
            if parent_same {
                self.counts.same_family_parent_child += 1;
                self.counts.per_family.entry(f).or_insert((0, 0)).1 += 1;
            }
        }
        self.stack.push(fam);
    }
}

impl Handler for NestedCounter {
    type Evm = BenchEvm;
    type Error = BenchError;
    type HaltReason = OpHaltReason;

    fn run_exec_loop(
        &mut self,
        evm: &mut Self::Evm,
        first_frame_input: revm::interpreter::interpreter_action::FrameInit,
    ) -> Result<FrameResult, Self::Error> {
        self.stack.clear();
        let res = evm.frame_init(first_frame_input)?;
        if let ItemOrResult::Result(frame_result) = res {
            return Ok(frame_result);
        }
        self.push_frame(evm);

        loop {
            let call_or_result = evm.frame_run()?;
            let (result, frame_completed) = match call_or_result {
                ItemOrResult::Item(init) => match evm.frame_init(init)? {
                    ItemOrResult::Item(_) => {
                        self.push_frame(evm);
                        continue;
                    }
                    ItemOrResult::Result(result) => (result, false),
                },
                ItemOrResult::Result(result) => (result, true),
            };

            if let Some(result) = evm.frame_return_result(result)? {
                if frame_completed {
                    self.stack.pop();
                }
                return Ok(result);
            }
            if frame_completed {
                self.stack.pop();
            }
        }
    }
}

fn replay_block(bench_dir: &Path, block: u64, family_of: &Arc<HashMap<B256, u64>>) -> Counts {
    let Ok(loader) = BinLoader::new(bench_dir, block) else { return Counts::default() };
    let chain_id = loader.raw_txs().first().and_then(|tx| tx.chain_id);
    let mut evm = make_evm(&loader, chain_id);
    let mut handler = NestedCounter::new(family_of.clone());

    for tx_bin in loader.raw_txs() {
        // Same convention as the exec-sanity replays: deposits are skipped.
        if tx_bin.tx_type == 0x7e {
            continue;
        }
        evm.0.ctx.tx = build_op_tx(tx_bin);
        let _ = handler.run(&mut evm);
        handler.counts.txs_replayed += 1;
    }
    handler.counts.blocks_replayed = 1;
    handler.counts
}

#[derive(Serialize)]
struct FamilyRow {
    skeleton_hash: String,
    ancestor_descendant: u64,
    parent_child: u64,
}

#[derive(Serialize)]
struct Output {
    skeleton_mode: String,
    start_block: u64,
    block_count: u64,
    blocks_replayed: u64,
    txs_replayed: u64,
    total_frames: u64,
    family_member_frames: u64,
    same_family_ancestor_descendant: u64,
    same_family_parent_child: u64,
    families_with_nesting: Vec<FamilyRow>,
}

fn main() {
    let args = Args::parse();
    let bench_dir = PathBuf::from(&args.bench_dir);

    eprintln!(
        "[nested_family_count] mode={} blocks {}..{} threads={}",
        args.skeleton_mode.as_str(),
        args.start_block,
        args.start_block + args.block_count,
        args.threads
    );

    let t0 = Instant::now();
    let mut contracts = collect_bytecodes_cached(&bench_dir, args.start_block, args.block_count);
    contracts.retain(|_, bytes| !skelot_common::should_skip_bytes(bytes));
    let grouping = group_skeleton_families(&contracts, args.skeleton_mode);
    let mut family_of: HashMap<B256, u64> = HashMap::new();
    for fam in &grouping.families {
        for member in &fam.members {
            family_of.insert(*member, fam.skel_hash);
        }
    }
    eprintln!(
        "  {} bytecodes, {} families ({} members) in {:.1}s",
        contracts.len(),
        grouping.families.len(),
        family_of.len(),
        t0.elapsed().as_secs_f64()
    );
    drop(contracts);
    let family_of = Arc::new(family_of);

    let t1 = Instant::now();
    let blocks: Vec<u64> = (args.start_block..args.start_block + args.block_count).collect();
    let chunks: Vec<Vec<u64>> = blocks.chunks(blocks.len().div_ceil(args.threads)).map(|c| c.to_vec()).collect();

    let totals = std::sync::Mutex::new(Counts::default());
    let done = std::sync::atomic::AtomicU64::new(0);
    std::thread::scope(|s| {
        for chunk in chunks {
            let family_of = family_of.clone();
            let bench_dir = bench_dir.clone();
            let totals = &totals;
            let done = &done;
            s.spawn(move || {
                let mut local = Counts::default();
                for block in chunk {
                    local.merge(replay_block(&bench_dir, block, &family_of));
                    let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    if n % 500 == 0 {
                        eprintln!("  [{n}] blocks replayed, {:.0}s", t1.elapsed().as_secs_f64());
                    }
                }
                totals.lock().unwrap().merge(local);
            });
        }
    });
    let counts = totals.into_inner().unwrap();

    let mut rows: Vec<FamilyRow> = counts
        .per_family
        .iter()
        .map(|(fam, (ad, pc))| FamilyRow {
            skeleton_hash: format!("{fam:016x}"),
            ancestor_descendant: *ad,
            parent_child: *pc,
        })
        .collect();
    rows.sort_by(|a, b| {
        b.ancestor_descendant
            .cmp(&a.ancestor_descendant)
            .then_with(|| a.skeleton_hash.cmp(&b.skeleton_hash))
    });

    println!("=== Nested same-family call frames ({}) ===", args.skeleton_mode.as_str());
    println!("  blocks replayed:              {}", counts.blocks_replayed);
    println!("  txs replayed:                 {}", counts.txs_replayed);
    println!("  total frames:                 {}", counts.total_frames);
    println!("  family-member frames:         {}", counts.family_member_frames);
    println!("  same-family ancestor/descend: {}", counts.same_family_ancestor_descendant);
    println!("  same-family direct parent:    {}", counts.same_family_parent_child);
    println!("  families with nesting:        {}", rows.len());
    for row in rows.iter().take(15) {
        println!(
            "    {}: ancestor={} parent={}",
            row.skeleton_hash, row.ancestor_descendant, row.parent_child
        );
    }
    println!("  replay wall: {:.1}s", t1.elapsed().as_secs_f64());

    if let Some(out) = &args.out {
        let output = Output {
            skeleton_mode: args.skeleton_mode.as_str().to_string(),
            start_block: args.start_block,
            block_count: args.block_count,
            blocks_replayed: counts.blocks_replayed,
            txs_replayed: counts.txs_replayed,
            total_frames: counts.total_frames,
            family_member_frames: counts.family_member_frames,
            same_family_ancestor_descendant: counts.same_family_ancestor_descendant,
            same_family_parent_child: counts.same_family_parent_child,
            families_with_nesting: rows,
        };
        let path = PathBuf::from(out);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&path, serde_json::to_vec_pretty(&output).expect("serialize")).expect("write out");
        eprintln!("wrote {}", path.display());
    }
}
