//! Frame-level runtime comparison: skeleton .so vs per-hash .so dispatch.
//! Measures per-frame execution time under both dispatch modes across sampled
//! blocks, producing paired Welch's t-test results per contract.

#[allow(dead_code)]
#[path = "../bin_common.rs"]
mod bin_common;

#[allow(dead_code)]
#[path = "../skelot_common.rs"]
mod skelot_common;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;
use op_revm::transaction::OpTransaction;
use op_revm::DefaultOp;
use revm::database::EmptyDB;
use revm::handler::Handler;
use revm::primitives::B256;
use revmc::emse::{bytecode_view, SkeletonMode};
use revmc::skeleton::{build_data_table, ClassifyMode};
use revmc::OptimizationLevel;
use revmc_builtins as _;
use revmc_context::{EvmCompilerFn, RawEvmCompilerFn};
use serde::Serialize;

use bin_common::{build_op_cfg, build_op_tx, extract_gas, BenchEvm, BinLoader, OpCtx};

const OPT: OptimizationLevel = OptimizationLevel::Aggressive;

#[derive(Parser)]
#[command(name = "skeleton_vs_perhash_bench")]
struct Cli {
    #[arg(long, default_value = "/home/ubuntu/sipeng/bench_data_base")]
    bench_dir: String,
    #[arg(long)]
    per_hash_cache: String,
    #[arg(long)]
    skeleton_cache: String,
    #[arg(long, default_value_t = 38004930)]
    start_block: u64,
    #[arg(long, default_value_t = 10000)]
    block_count: u64,
    #[arg(long, default_value_t = 100)]
    sample_every: u64,
    #[arg(long, default_value_t = 5)]
    rounds: usize,
    #[arg(long, default_value_t = 2)]
    warmup: usize,
    #[arg(long, default_value_t = 0.05)]
    alpha: f64,
    #[arg(long)]
    out: Option<String>,
    /// Load pre-built skeleton registry instead of scanning all blocks.
    /// Saves ~5 min on repeat runs. Generated automatically on first run.
    #[arg(long)]
    registry: Option<String>,
    /// Restrict measurement loop to a single dispatch mode for clean per-mode
    /// hardware-counter wraps. "both" (default) preserves the original mixed-mode
    /// behavior used to produce skeleton_vs_perhash.json.
    #[arg(long, default_value = "both")]
    only_mode: String,
    /// If set, bracket every JIT call on a target bytecode_hash with
    /// perf_event_open ioctl reads and dump per-hash counters to this path.
    /// Default event set: cycles, instructions, L1i miss, iTLB miss,
    /// branches, branch-misses. See --perf-events for alternative groups.
    #[arg(long)]
    perf_out: Option<String>,
    /// Which 6-event perf group to bracket each JIT call with.
    ///   default  — cycles, instructions, L1i miss, iTLB miss, branches, branch-misses (Hardware/Cache events)
    ///   frontend — cycles, instructions, icache_data.stalls, icache_tag.stalls, dsb2mite_switches.penalty_cycles, cycle_activity.stalls_total (RAW configs for Sapphire Rapids)
    ///   backend  — cycles, instructions, cycle_activity.stalls_total, cycle_activity.stalls_l1d_miss, idq.dsb_uops, idq.mite_uops (RAW configs)
    ///   tma_l1   — cycles, instructions, TMA slots, retiring slots, bad-spec slots, backend-bound slots (RAW configs)
    ///   tma_l2_bw — cycles, instructions, IDQ DSB/MITE any/ok cycles (frontend bandwidth diagnostics)
    ///   tma_l3_fetch — cycles, instructions, I-cache stalls plus L2/offcore code-read diagnostics
    #[arg(long, default_value = "default")]
    perf_events: String,
    /// SkelOT classification mode: "auto" (default — most PUSHes hardcoded
    /// as movq-imm, only structurally-variant PUSHes go via data table) or
    /// "force-all-variant" (every PUSH is a data-table load). Used together
    /// with `--skeleton-cache` pointing at a directory of `skelot_forced.so`
    /// renamed to `skel_<short>__prague__o3.so` to test whether SkelOT's
    /// data-table cost B is small in the cross-instance regime (P3).
    #[arg(long, default_value = "auto")]
    classify_mode: ClassifyMode,
    /// Family boundary: "current" (PUSH-immediates-only skeleton) or "emse"
    /// (also strips CBOR metadata + trailing zeros; admission guards apply).
    /// Must match the mode used to compile `--skeleton-cache`.
    #[arg(long, default_value = "current")]
    skeleton_mode: SkeletonMode,
    /// Include per-(block,round) raw samples in per_contract records. Off by
    /// default: at --sample-every 1 the raw list dominates output size.
    #[arg(long, default_value_t = false)]
    emit_raw_samples: bool,
    /// Proceed even when some skeleton .so files fail to load. Default is a
    /// hard abort: a silently missing artifact would fall back to the
    /// per-hash function, biasing the skeleton arm.
    #[arg(long, default_value_t = false)]
    allow_missing_skeletons: bool,
    /// interpreter mode only: time EVERY corpus contract (family members AND
    /// per-hash solos) instead of family members only. Used to weigh
    /// compile units by exclusive own-time over a block window ([HO-COV]).
    #[arg(long, default_value_t = false)]
    all_corpus_targets: bool,
    /// Dump per-(block, code_hash) exclusive-time samples as TSV
    /// (block \t hash \t microseconds), written AFTER the timed loop.
    /// Preserves the block axis the aggregated JSON folds away, so any
    /// window split (halves, quartiles, sliding) becomes offline work.
    #[arg(long)]
    per_block_out: Option<String>,
}

#[path = "../perf_events.rs"]
mod perf_events;
use perf_events::{perf_event_names, perf_init, PerfEventSet, PERF_NEVENTS};

#[path = "../vs_perhash_report.rs"]
mod vs_perhash_report;
#[path = "../vs_perhash_families.rs"]
mod vs_perhash_families;
use vs_perhash_families::{load_or_build_families, AnalyzedGroup};
use vs_perhash_report::{
    cohens_d, BlockSummary, Config, ContractRecord, FamilyAggregate, ModeReport, Output,
    RawSample, Summary, TargetAggregate,
};

fn load_skeleton_so(
    path: &Path,
    symbol: &str,
) -> Result<(RawEvmCompilerFn, libloading::Library), String> {
    let library = unsafe { libloading::Library::new(path) }.map_err(|e| format!("dlopen: {e}"))?;
    // Try the requested symbol first (skel_<short> for normal skelot artifacts,
    // c_<hash> for per_hash artifacts). Fall back to the names used by
    // skelot_ablation_bench's stage_e cache so we can drop those .so files
    // into a renamed dir for cross-instance B comparisons (P3).
    let candidates = [symbol, "skelot_forced", "skelot_auto"];
    for cand in candidates {
        if let Ok(sym) = unsafe { library.get::<RawEvmCompilerFn>(cand.as_bytes()) } {
            let function = *sym;
            drop(sym);
            return Ok((function, library));
        }
    }
    Err(format!("dlsym: none of {candidates:?} found"))
}

#[path = "../timing_handlers.rs"]
mod timing_handlers;
use timing_handlers::{TimingInterpreterHandler, TimingPerHashHandler, TimingSkeletonHandler};

fn execute_block_timed(
    loader: &BinLoader,
    chain_id: Option<u64>,
    mut run_tx: impl FnMut(&mut BenchEvm) -> (bool, u64),
) -> (Vec<(bool, u64)>, std::time::Duration) {
    let cfg = build_op_cfg(chain_id);
    let dummy_tx = OpTransaction::builder().build_fill();
    let mut evm: BenchEvm = {
        let ctx =
            OpCtx::<EmptyDB>::op().with_cfg(cfg).with_db(loader.build_cache_db()).with_tx(dummy_tx);
        op_revm::OpEvm::new(ctx, ())
    };
    let mut results = Vec::with_capacity(loader.tx_count());
    // Exec wall: tx loop only — loader IO and CacheDB construction excluded.
    let exec_t0 = Instant::now();
    for tx_bin in loader.raw_txs() {
        if tx_bin.tx_type == 0x7e {
            results.push((true, 0));
            continue;
        }
        evm.0.ctx.tx = build_op_tx(tx_bin);
        results.push(run_tx(&mut evm));
    }
    (results, exec_t0.elapsed())
}

fn main() {
    let cli = Cli::parse();
    let bench_dir = Path::new(&cli.bench_dir);
    let classify_mode = cli.classify_mode;
    let per_hash_cache = PathBuf::from(&cli.per_hash_cache);
    let skeleton_cache = PathBuf::from(&cli.skeleton_cache);

    let block_numbers: Vec<u64> = (0..cli.block_count)
        .step_by((cli.sample_every as usize).max(1))
        .map(|i| cli.start_block + i)
        .collect();

    // Mode gating up front: the interpreter arm needs neither artifact set,
    // so skip both dlopen phases (loading 19K .so files costs minutes and GBs).
    let mode_per_hash = matches!(cli.only_mode.as_str(), "both" | "per_hash");
    let mode_skeleton = matches!(cli.only_mode.as_str(), "both" | "skeleton");
    let mode_interpreter = cli.only_mode.as_str() == "interpreter";
    if !mode_per_hash && !mode_skeleton && !mode_interpreter {
        eprintln!(
            "ERROR: --only-mode must be one of: both, per_hash, skeleton, interpreter (got {})",
            cli.only_mode
        );
        std::process::exit(2);
    }

    println!("=== Skeleton vs Per-Hash Frame-Level Benchmark ===");
    println!(
        "Blocks: {} sampled ({} to {}, every {})",
        block_numbers.len(),
        block_numbers.first().unwrap_or(&0),
        block_numbers.last().unwrap_or(&0),
        cli.sample_every,
    );
    println!("Rounds: {} measured, {} warmup\n", cli.rounds, cli.warmup);

    // Phase 1: Load all per-hash .so files (the skeleton handler also needs
    // them for its non-family fallback dispatch).
    let need_per_hash_fns = mode_per_hash || mode_skeleton;
    println!("--- Phase 1: Loading per-hash .so files ---");
    let t0 = Instant::now();
    let mut per_hash_fns: HashMap<B256, RawEvmCompilerFn> = HashMap::new();
    let mut per_hash_libs: Vec<libloading::Library> = Vec::new();

    let per_hash_entries: Vec<_> = if need_per_hash_fns {
        std::fs::read_dir(&per_hash_cache)
            .expect("read per-hash cache dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map_or(false, |ext| ext == "so" || ext == "dylib"))
            .collect()
    } else {
        println!("  (skipped — interpreter mode)");
        Vec::new()
    };

    for entry in &per_hash_entries {
        let path = entry.path();
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let parts: Vec<&str> = stem.split("__").collect();
        if parts.is_empty() || parts[0].len() != 64 {
            continue;
        }
        let hash_hex = parts[0];
        let hash = match hex::decode(hash_hex) {
            Ok(bytes) if bytes.len() == 32 => B256::from_slice(&bytes),
            _ => continue,
        };
        let symbol = format!("c_{hash_hex}");
        match load_skeleton_so(&path, &symbol) {
            Ok((f, lib)) => {
                per_hash_fns.insert(hash, f);
                per_hash_libs.push(lib);
            }
            Err(_) => {}
        }
    }
    println!(
        "  Loaded {} per-hash functions in {:.2}s",
        per_hash_fns.len(),
        t0.elapsed().as_secs_f64()
    );

    // Phase 2: Load or reconstruct skeleton families (see vs_perhash_families.rs).
    let t1 = Instant::now();
    let analyzed: Vec<AnalyzedGroup> = load_or_build_families(
        cli.registry.as_deref(),
        cli.out.as_deref(),
        bench_dir,
        cli.start_block,
        cli.block_count,
        cli.skeleton_mode,
        classify_mode,
        t1,
    );


    // Phase 3: Load skeleton .so files and build dispatch tables
    println!("\n--- Phase 3: Loading skeleton .so files ---");
    let t2 = Instant::now();

    let mut skeleton_dispatch: HashMap<B256, (EvmCompilerFn, Vec<u8>)> = HashMap::new();
    let mut skeleton_libs: Vec<libloading::Library> = Vec::new();
    let mut skeleton_hash_map: HashMap<B256, (u64, usize)> = HashMap::new();
    let mut skeleton_load_failures = 0usize;

    if mode_skeleton {
        for g in &analyzed {
            let arts = skelot_common::skeleton_paths(&skeleton_cache, g.skel_hash, OPT);
            match load_skeleton_so(&arts.library, &arts.symbol) {
                Ok((raw_fn, lib)) => {
                    skeleton_libs.push(lib);
                    let skel_fn = EvmCompilerFn::new(raw_fn);
                    if let Some(ref cached_tables) = g.cached_data_tables {
                        // Use pre-computed data tables from registry
                        for (hash, dt) in cached_tables {
                            skeleton_dispatch.insert(*hash, (skel_fn, dt.clone()));
                            skeleton_hash_map.insert(*hash, (g.skel_hash, g.members.len()));
                        }
                    } else {
                        // Build data tables from the mode's bytecode view
                        for (hash, bytes) in &g.members {
                            let table = build_data_table(bytecode_view(bytes, cli.skeleton_mode), &g.variance);
                            skeleton_dispatch.insert(*hash, (skel_fn, table.data));
                            skeleton_hash_map.insert(*hash, (g.skel_hash, g.members.len()));
                        }
                    }
                }
                Err(e) => {
                    skeleton_load_failures += 1;
                    eprintln!("  WARN: skeleton {:016x} load failed: {e}", g.skel_hash);
                }
            }
        }
        // A missing artifact silently falls back to the per-hash function in
        // the skeleton handler — untimed, uncounted, and biased in the
        // skeleton arm's favour. Account for every family or refuse to run.
        if skeleton_load_failures > 0 {
            eprintln!(
                "  ERROR: {skeleton_load_failures}/{} skeleton artifacts failed to load",
                analyzed.len()
            );
            if !cli.allow_missing_skeletons {
                eprintln!("  aborting (pass --allow-missing-skeletons to override)");
                std::process::exit(1);
            }
        }
    } else {
        println!("  (skipped — mode {})", cli.only_mode);
    }

    println!(
        "  {} skeleton-dispatched contracts in {:.2}s",
        skeleton_dispatch.len(),
        t2.elapsed().as_secs_f64()
    );

    // Phase 4: Identify treatment group — the same family-member universe in
    // every arm, intersected with whichever artifact sets the arm dispatches.
    let target_hashes: HashSet<B256> = if mode_interpreter {
        if cli.all_corpus_targets {
            skelot_common::collect_bytecodes_cached(bench_dir, cli.start_block, cli.block_count)
                .keys()
                .copied()
                .collect()
        } else {
            analyzed.iter().flat_map(|g| g.members.iter().map(|(h, _)| *h)).collect()
        }
    } else if mode_skeleton {
        skeleton_dispatch.keys().filter(|h| per_hash_fns.contains_key(*h)).copied().collect()
    } else {
        // per_hash-only: family members with a per-hash artifact.
        analyzed
            .iter()
            .flat_map(|g| g.members.iter().map(|(h, _)| *h))
            .filter(|h| per_hash_fns.contains_key(h))
            .collect()
    };

    println!("\n--- Treatment group: {} contracts ---", target_hashes.len());

    if target_hashes.is_empty() {
        eprintln!("No contracts in treatment group. Nothing to compare.");
        std::process::exit(1);
    }

    let per_hash_fns = Arc::new(per_hash_fns);
    let skeleton_dispatch = Arc::new(skeleton_dispatch);
    let target_hashes = Arc::new(target_hashes);

    // Phase 5: Run blocks with timing
    println!(
        "\n--- Phase 5: Measuring {} blocks (only_mode={}) ---",
        block_numbers.len(),
        cli.only_mode
    );
    let record_perf = cli.perf_out.is_some();
    let perf_set = if record_perf {
        match PerfEventSet::parse(&cli.perf_events) {
            Some(s) => s,
            None => {
                eprintln!(
                    "ERROR: --perf-events must be one of: default, frontend, backend, tma_l1, tma_l2_bw, tma_l3_fetch (got {})",
                    cli.perf_events
                );
                std::process::exit(2);
            }
        }
    } else {
        PerfEventSet::Default
    };
    if record_perf {
        perf_init(perf_set);
    }
    eprintln!("MEASUREMENT_PHASE_BEGIN only_mode={}", cli.only_mode);

    let mut per_hash_samples: HashMap<B256, Vec<(u64, usize, f64)>> = HashMap::new();
    let mut skeleton_samples: HashMap<B256, Vec<(u64, usize, f64)>> = HashMap::new();
    let mut interpreter_samples: HashMap<B256, Vec<(u64, usize, f64)>> = HashMap::new();
    let mut per_hash_perf: HashMap<B256, [u64; PERF_NEVENTS + 1]> = HashMap::new();
    let mut skeleton_perf: HashMap<B256, [u64; PERF_NEVENTS + 1]> = HashMap::new();
    let mut per_hash_wall_ns: HashMap<B256, u128> = HashMap::new();
    let mut skeleton_wall_ns: HashMap<B256, u128> = HashMap::new();
    let mut blocks_measured = 0usize;
    let mut blocks_skipped = 0usize;
    let mut correctness_blocks_rejected = 0usize;
    let mut correctness_tx_mismatches = 0u64;

    #[derive(Default)]
    struct ArmStats {
        exec_wall_s: f64,
        tx_count: u64,
        success_count: u64,
        total_gas: u64,
    }
    impl ArmStats {
        fn absorb_round0(&mut self, results: &[(bool, u64)]) {
            self.tx_count += results.len() as u64;
            self.success_count += results.iter().filter(|(ok, _)| *ok).count() as u64;
            self.total_gas += results.iter().map(|(_, g)| *g).sum::<u64>();
        }
    }
    let mut ph_arm = ArmStats::default();
    let mut sk_arm = ArmStats::default();
    let mut int_arm = ArmStats::default();

    for (bi, &block) in block_numbers.iter().enumerate() {
        let loader = match BinLoader::new(bench_dir, block) {
            Ok(l) => l,
            Err(_) => {
                blocks_skipped += 1;
                continue;
            }
        };
        let chain_id = loader.raw_txs().first().and_then(|tx| tx.chain_id);

        // Correctness check: only run when comparing both modes; for single-mode
        // hardware-counter wraps the cross-check would double the work and bias
        // perf totals (and is already covered by mixed-mode runs of this bench).
        if mode_per_hash && mode_skeleton {
            let mut ph_check = TimingPerHashHandler {
                functions: per_hash_fns.clone(),
                target_hashes: target_hashes.clone(),
                timings: HashMap::new(),
                perf_counts: HashMap::new(),
            };
            let (ph_results, _) =
                execute_block_timed(&loader, chain_id, |evm| match ph_check.run(evm) {
                    Ok(rr) => extract_gas(&rr),
                    Err(_) => (false, 0),
                });
            let mut sk_check = TimingSkeletonHandler {
                per_hash_fns: per_hash_fns.clone(),
                skeleton_dispatch: skeleton_dispatch.clone(),
                target_hashes: target_hashes.clone(),
                timings: HashMap::new(),
                perf_counts: HashMap::new(),
            };
            let (sk_results, _) =
                execute_block_timed(&loader, chain_id, |evm| match sk_check.run(evm) {
                    Ok(rr) => extract_gas(&rr),
                    Err(_) => (false, 0),
                });
            let mut mismatches = 0u32;
            for (i, (ph, sk)) in ph_results.iter().zip(sk_results.iter()).enumerate() {
                if ph != sk {
                    mismatches += 1;
                    eprintln!(
                        "  MISMATCH block {} tx[{}]: per-hash=({},{}) skeleton=({},{})",
                        block, i, ph.0, ph.1, sk.0, sk.1
                    );
                }
            }
            if mismatches > 0 {
                eprintln!("  ABORT block {}: {} mismatches — skipping", block, mismatches);
                correctness_blocks_rejected += 1;
                correctness_tx_mismatches += u64::from(mismatches);
                continue;
            }
        }

        if mode_per_hash {
            // Per-hash warmup
            for _ in 0..cli.warmup {
                execute_block_timed(&loader, chain_id, |evm| {
                    let mut h = TimingPerHashHandler {
                        functions: per_hash_fns.clone(),
                        target_hashes: target_hashes.clone(),
                        timings: HashMap::new(),
                        perf_counts: HashMap::new(),
                    };
                    match h.run(evm) {
                        Ok(rr) => extract_gas(&rr),
                        Err(_) => (false, 0),
                    }
                });
            }

            // Per-hash measured rounds
            for round_idx in 0..cli.rounds {
                let mut handler = TimingPerHashHandler {
                    functions: per_hash_fns.clone(),
                    target_hashes: target_hashes.clone(),
                    timings: HashMap::new(),
                    perf_counts: HashMap::new(),
                };
                let (results, exec_dur) =
                    execute_block_timed(&loader, chain_id, |evm| match handler.run(evm) {
                        Ok(rr) => extract_gas(&rr),
                        Err(_) => (false, 0),
                    });
                ph_arm.exec_wall_s += exec_dur.as_secs_f64();
                if round_idx == 0 {
                    ph_arm.absorb_round0(&results);
                }
                for (hash, dur) in &handler.timings {
                    per_hash_samples.entry(*hash).or_default().push((
                        block,
                        round_idx,
                        dur.as_nanos() as f64 / 1000.0,
                    ));
                    if record_perf {
                        *per_hash_wall_ns.entry(*hash).or_insert(0) += dur.as_nanos();
                    }
                }
                if record_perf {
                    for (hash, counts) in &handler.perf_counts {
                        let agg = per_hash_perf.entry(*hash).or_insert([0u64; PERF_NEVENTS + 1]);
                        for i in 0..=PERF_NEVENTS {
                            agg[i] = agg[i].wrapping_add(counts[i]);
                        }
                    }
                }
            }
        }

        if mode_skeleton {
            // Skeleton warmup
            for _ in 0..cli.warmup {
                execute_block_timed(&loader, chain_id, |evm| {
                    let mut h = TimingSkeletonHandler {
                        per_hash_fns: per_hash_fns.clone(),
                        skeleton_dispatch: skeleton_dispatch.clone(),
                        target_hashes: target_hashes.clone(),
                        timings: HashMap::new(),
                        perf_counts: HashMap::new(),
                    };
                    match h.run(evm) {
                        Ok(rr) => extract_gas(&rr),
                        Err(_) => (false, 0),
                    }
                });
            }

            // Skeleton measured rounds
            for round_idx in 0..cli.rounds {
                let mut handler = TimingSkeletonHandler {
                    per_hash_fns: per_hash_fns.clone(),
                    skeleton_dispatch: skeleton_dispatch.clone(),
                    target_hashes: target_hashes.clone(),
                    timings: HashMap::new(),
                    perf_counts: HashMap::new(),
                };
                let (results, exec_dur) =
                    execute_block_timed(&loader, chain_id, |evm| match handler.run(evm) {
                        Ok(rr) => extract_gas(&rr),
                        Err(_) => (false, 0),
                    });
                sk_arm.exec_wall_s += exec_dur.as_secs_f64();
                if round_idx == 0 {
                    sk_arm.absorb_round0(&results);
                }
                for (hash, dur) in &handler.timings {
                    skeleton_samples.entry(*hash).or_default().push((
                        block,
                        round_idx,
                        dur.as_nanos() as f64 / 1000.0,
                    ));
                    if record_perf {
                        *skeleton_wall_ns.entry(*hash).or_insert(0) += dur.as_nanos();
                    }
                }
                if record_perf {
                    for (hash, counts) in &handler.perf_counts {
                        let agg = skeleton_perf.entry(*hash).or_insert([0u64; PERF_NEVENTS + 1]);
                        for i in 0..=PERF_NEVENTS {
                            agg[i] = agg[i].wrapping_add(counts[i]);
                        }
                    }
                }
            }
        }

        if mode_interpreter {
            for _ in 0..cli.warmup {
                execute_block_timed(&loader, chain_id, |evm| {
                    let mut h = TimingInterpreterHandler {
                        target_hashes: target_hashes.clone(),
                        timings: HashMap::new(),
                        perf_counts: HashMap::new(),
                    };
                    match h.run(evm) {
                        Ok(rr) => extract_gas(&rr),
                        Err(_) => (false, 0),
                    }
                });
            }

            for round_idx in 0..cli.rounds {
                let mut handler = TimingInterpreterHandler {
                    target_hashes: target_hashes.clone(),
                    timings: HashMap::new(),
                    perf_counts: HashMap::new(),
                };
                let (results, exec_dur) =
                    execute_block_timed(&loader, chain_id, |evm| match handler.run(evm) {
                        Ok(rr) => extract_gas(&rr),
                        Err(_) => (false, 0),
                    });
                int_arm.exec_wall_s += exec_dur.as_secs_f64();
                if round_idx == 0 {
                    int_arm.absorb_round0(&results);
                }
                for (hash, dur) in &handler.timings {
                    interpreter_samples.entry(*hash).or_default().push((
                        block,
                        round_idx,
                        dur.as_nanos() as f64 / 1000.0,
                    ));
                }
            }
        }

        blocks_measured += 1;
        if (bi + 1) % 10 == 0 || bi + 1 == block_numbers.len() {
            println!("  [{}/{}] block {} done", bi + 1, block_numbers.len(), block);
        }
    }

    eprintln!("MEASUREMENT_PHASE_END only_mode={}", cli.only_mode);
    println!("\n  {blocks_measured} blocks measured, {blocks_skipped} skipped (unreadable)");

    // [JUMP-HIT]: jumps the metadata mask rejected that vanilla semantics
    // would have accepted. Only interpreter-executed frames consult the
    // legacy jump table, so the count is meaningful ONLY in the interpreter
    // arm — JIT arms structurally under-count and must not cite it.
    let jump_hits = revm::interpreter::emse_jump_audit::masked_jump_hits();
    println!(
        "  [JUMP-HIT] masked_jump_hits = {jump_hits}{}",
        if mode_interpreter { "" } else { " (non-interpreter arm: not meaningful)" }
    );

    // Phase 6: Aggregate and statistical testing
    println!("\n--- Phase 6: Aggregation ---");

    let mut records: Vec<ContractRecord> = Vec::new();
    let mut n_faster = 0usize;
    let mut n_equiv = 0usize;
    let mut n_slower = 0usize;
    let mut all_ratios: Vec<f64> = Vec::new();
    let mut all_deltas: Vec<f64> = Vec::new();

    let all_measured: HashSet<B256> =
        per_hash_samples.keys().filter(|h| skeleton_samples.contains_key(*h)).copied().collect();
    let mut block_data: HashMap<u64, Vec<f64>> = HashMap::new();

    for hash in &all_measured {
        let ph = &per_hash_samples[hash];
        let sk = &skeleton_samples[hash];
        if ph.len() < 2 || sk.len() < 2 {
            continue;
        }

        // Pair by (block, round): the two arms measure the SAME contract in
        // the SAME replay slot, so the difference is the quantity of
        // interest. An unpaired test would keep the between-block workload
        // spread in the denominator and treat repeated rounds of one block
        // as independent observations.
        let sk_by_slot: HashMap<(u64, usize), f64> =
            sk.iter().map(|&(b, r, us)| ((b, r), us)).collect();
        let mut ph_vals: Vec<f64> = Vec::with_capacity(ph.len());
        let mut sk_vals: Vec<f64> = Vec::with_capacity(ph.len());
        for &(b, r, us) in ph {
            if let Some(&sk_us) = sk_by_slot.get(&(b, r)) {
                ph_vals.push(us);
                sk_vals.push(sk_us);
            }
        }
        if ph_vals.len() < 2 {
            continue;
        }

        let mean_ph = ph_vals.iter().sum::<f64>() / ph_vals.len() as f64;
        let mean_sk = sk_vals.iter().sum::<f64>() / sk_vals.len() as f64;
        let delta = mean_sk - mean_ph;
        let ratio = if mean_sk > 0.0 { mean_ph / mean_sk } else { 1.0 };

        let (_, p) = skelot_common::stats::paired_t(&ph_vals, &sk_vals);
        let d = cohens_d(&ph_vals, &sk_vals);

        let verdict = if p >= cli.alpha {
            n_equiv += 1;
            "EQUIVALENT"
        } else if delta < 0.0 {
            n_faster += 1;
            "SKELETON_FASTER"
        } else {
            n_slower += 1;
            "SKELETON_SLOWER"
        };

        let (skel_hash, family_size) = skeleton_hash_map.get(hash).copied().unwrap_or((0, 0));

        // Indexed join: at --sample-every 1 the former linear scan per
        // element was O(n²) per hash and dominated Phase 6.
        let sk_index: HashMap<(u64, usize), f64> =
            sk.iter().map(|&(b, r, us)| ((b, r), us)).collect();
        let mut raw: Vec<RawSample> = Vec::new();
        for &(blk, rnd, ph_us) in ph {
            if let Some(&sk_us) = sk_index.get(&(blk, rnd)) {
                raw.push(RawSample {
                    block: blk,
                    round: rnd,
                    per_hash_us: ph_us,
                    skeleton_us: sk_us,
                });
            }
        }

        all_ratios.push(ratio);
        all_deltas.push(delta);

        // Per-block ratios accumulate here so the per_block summary survives
        // even when raw samples are not emitted into the JSON.
        for s in &raw {
            if s.skeleton_us > 0.0 {
                block_data.entry(s.block).or_default().push(s.per_hash_us / s.skeleton_us);
            }
        }

        records.push(ContractRecord {
            code_hash: format!("0x{}", hex::encode(hash)),
            skeleton_hash: format!("skel_{skel_hash:016x}"),
            family_size,
            blocks_seen: ph.len(),
            mean_per_hash_us: mean_ph,
            mean_skeleton_us: mean_sk,
            delta_us: delta,
            speedup_ratio: ratio,
            welch_p: p,
            cohens_d: d,
            verdict: verdict.to_string(),
            raw_samples: if cli.emit_raw_samples { raw } else { Vec::new() },
        });
    }

    records.sort_by(|a, b| {
        a.delta_us
            .abs()
            .partial_cmp(&b.delta_us.abs())
            .unwrap_or(std::cmp::Ordering::Equal)
            .reverse()
    });

    // Per-family aggregates
    let mut family_map: HashMap<String, Vec<&ContractRecord>> = HashMap::new();
    for rec in &records {
        family_map.entry(rec.skeleton_hash.clone()).or_default().push(rec);
    }
    let fcmp = |a: &f64, b: &f64| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal);
    let mut per_family: Vec<FamilyAggregate> = family_map
        .into_iter()
        .map(|(skel_hash, members)| {
            let mut speedups: Vec<f64> = members.iter().map(|r| r.speedup_ratio).collect();
            speedups.sort_by(fcmp);
            let median_speedup = speedups[speedups.len() / 2];
            let mean_delta = members.iter().map(|r| r.delta_us).sum::<f64>() / members.len() as f64;
            let n = members.len() as f64;
            let n_f = members.iter().filter(|r| r.verdict == "SKELETON_FASTER").count() as f64;
            let n_e = members.iter().filter(|r| r.verdict == "EQUIVALENT").count() as f64;
            let n_s = members.iter().filter(|r| r.verdict == "SKELETON_SLOWER").count() as f64;
            FamilyAggregate {
                skeleton_hash: skel_hash,
                family_size: members[0].family_size,
                members_measured: members.len(),
                median_speedup,
                mean_delta_us: mean_delta,
                pct_faster: n_f / n * 100.0,
                pct_equivalent: n_e / n * 100.0,
                pct_slower: n_s / n * 100.0,
            }
        })
        .collect();
    per_family.sort_by(|a, b| b.members_measured.cmp(&a.members_measured));

    // Per-block breakdown (accumulated during the per-hash loop above)
    let mut per_block: Vec<BlockSummary> = block_data
        .into_iter()
        .map(|(block, mut ratios)| {
            ratios.sort_by(fcmp);
            let median = ratios[ratios.len() / 2];
            let mean_d = ratios.iter().map(|r| r - 1.0).sum::<f64>() / ratios.len() as f64;
            BlockSummary {
                block,
                contracts_measured: ratios.len(),
                median_speedup: median,
                mean_delta_us: mean_d,
            }
        })
        .collect();
    per_block.sort_by_key(|b| b.block);

    all_ratios.sort_by(fcmp);
    all_deltas.sort_by(fcmp);

    let median_ratio = if all_ratios.is_empty() { 1.0 } else { all_ratios[all_ratios.len() / 2] };
    let mean_delta = if all_deltas.is_empty() {
        0.0
    } else {
        all_deltas.iter().sum::<f64>() / all_deltas.len() as f64
    };
    let p95_delta = if all_deltas.is_empty() {
        0.0
    } else {
        // Nearest-rank p95: the ceil(0.95n)-th value, 1-based, hence
        // index ceil(0.95n)-1. The previous form used ceil(0.95n) as a
        // 0-based index (one rank high) and degenerated to the maximum
        // for n <= 39.
        let rank = ((all_deltas.len() as f64 * 0.95).ceil() as usize).max(1);
        all_deltas[(rank - 1).min(all_deltas.len() - 1)]
    };

    let total = records.len();
    println!("\n=== Results ({total} contracts measured) ===");
    println!("  SKELETON_FASTER:  {n_faster}");
    println!("  EQUIVALENT:       {n_equiv}");
    println!("  SKELETON_SLOWER:  {n_slower}");
    println!("  Median speedup ratio: {median_ratio:.4}");
    println!("  Mean delta (us):      {mean_delta:.2}");
    println!("  P95 delta (us):       {p95_delta:.2}");
    println!("  Families: {}  Blocks: {}", per_family.len(), per_block.len());

    if let Some(pb_path) = &cli.per_block_out {
        use std::io::Write;
        let mut f = std::io::BufWriter::new(
            std::fs::File::create(pb_path).expect("create per-block out"),
        );
        let mut rows = 0usize;
        for (label, samples) in [
            ("per_hash", &per_hash_samples),
            ("skelot", &skeleton_samples),
            ("interpreter", &interpreter_samples),
        ] {
            for (hash, v) in samples {
                for (block, _round, us) in v {
                    writeln!(f, "{label}\t{block}\t0x{}\t{us:.3}", hex::encode(hash))
                        .expect("write per-block row");
                    rows += 1;
                }
            }
        }
        println!("  per-block samples written: {rows} rows -> {pb_path}");
    }

    // Per-mode reports: unconditional, so single-arm runs keep their wall
    // clock, replay totals, and per-target aggregates even though the paired
    // per_contract analysis above is empty.
    let build_mode_report = |mode: &str,
                             arm: &ArmStats,
                             samples: &HashMap<B256, Vec<(u64, usize, f64)>>,
                             jump: Option<u64>| {
        let mut per_target: Vec<TargetAggregate> = samples
            .iter()
            .map(|(h, v)| TargetAggregate {
                code_hash: format!("0x{}", hex::encode(h)),
                samples: v.len(),
                total_us: v.iter().map(|(_, _, us)| *us).sum(),
            })
            .collect();
        per_target.sort_by(|a, b| {
            b.total_us.partial_cmp(&a.total_us).unwrap_or(std::cmp::Ordering::Equal)
        });
        ModeReport {
            mode: mode.to_string(),
            exec_wall_seconds: arm.exec_wall_s,
            blocks_replayed: blocks_measured,
            blocks_skipped_unreadable: blocks_skipped,
            tx_count: arm.tx_count,
            success_count: arm.success_count,
            total_gas: arm.total_gas,
            samples_total: samples.values().map(|v| v.len()).sum(),
            targets_hit: samples.len(),
            masked_jump_hits: jump,
            per_target,
        }
    };
    let mut mode_reports: Vec<ModeReport> = Vec::new();
    if mode_per_hash {
        mode_reports.push(build_mode_report("per_hash", &ph_arm, &per_hash_samples, None));
    }
    if mode_skeleton {
        mode_reports.push(build_mode_report("skelot", &sk_arm, &skeleton_samples, None));
    }
    if mode_interpreter {
        mode_reports.push(build_mode_report(
            "interpreter",
            &int_arm,
            &interpreter_samples,
            Some(jump_hits),
        ));
    }
    for mr in &mode_reports {
        println!(
            "  mode {}: exec_wall {:.1}s, {} txs ({} ok, gas {}), {} targets, {} samples",
            mr.mode,
            mr.exec_wall_seconds,
            mr.tx_count,
            mr.success_count,
            mr.total_gas,
            mr.targets_hit,
            mr.samples_total
        );
    }

    if let Some(ref out_path) = cli.out {
        let output = Output {
            config: Config {
                blocks_sampled: blocks_measured,
                rounds: cli.rounds,
                warmup: cli.warmup,
                alpha: cli.alpha,
            },
            summary: Summary {
                total_family_members_measured: total,
                skeleton_faster: n_faster,
                equivalent: n_equiv,
                skeleton_slower: n_slower,
                median_speedup_ratio: median_ratio,
                mean_delta_us: mean_delta,
                p95_delta_us: p95_delta,
                correctness_blocks_rejected,
                correctness_tx_mismatches,
            },
            mode_reports,
            per_family,
            per_block,
            per_contract: records,
        };
        std::fs::write(out_path, serde_json::to_vec_pretty(&output).expect("serialize"))
            .expect("write output");
        println!("\nWrote {out_path}");
    }

    if let Some(ref perf_out) = cli.perf_out {
        // Generic per-target row. cycles + instructions are always present;
        // e2..e5 hold the four extra events whose semantics are defined by
        // event_order at the PerModePerf level. Legacy fields (l1i_misses
        // etc.) are also emitted *only* when --perf-events default for
        // backward compat with parsers that predate --perf-events.
        #[derive(Serialize)]
        struct PerfRow {
            code_hash: String,
            calls: u64,
            cycles: u64,
            instructions: u64,
            e2: u64,
            e3: u64,
            e4: u64,
            e5: u64,
            wall_ns: u128,
            // Legacy named slots, populated only in default event set.
            #[serde(skip_serializing_if = "Option::is_none")]
            l1i_misses: Option<u64>,
            #[serde(skip_serializing_if = "Option::is_none")]
            itlb_misses: Option<u64>,
            #[serde(skip_serializing_if = "Option::is_none")]
            branches: Option<u64>,
            #[serde(skip_serializing_if = "Option::is_none")]
            branch_misses: Option<u64>,
        }
        #[derive(Serialize)]
        struct PerModePerf {
            event_set: String,
            event_order: [&'static str; PERF_NEVENTS],
            n_target_hashes: usize,
            total_calls: u64,
            total_cycles: u64,
            total_instructions: u64,
            total_e2: u64,
            total_e3: u64,
            total_e4: u64,
            total_e5: u64,
            total_wall_ns: u128,
            #[serde(skip_serializing_if = "Option::is_none")]
            total_l1i_misses: Option<u64>,
            #[serde(skip_serializing_if = "Option::is_none")]
            total_itlb_misses: Option<u64>,
            #[serde(skip_serializing_if = "Option::is_none")]
            total_branches: Option<u64>,
            #[serde(skip_serializing_if = "Option::is_none")]
            total_branch_misses: Option<u64>,
            per_hash: Vec<PerfRow>,
        }
        #[derive(Serialize)]
        struct PerfOut {
            only_mode: String,
            blocks_sampled: usize,
            rounds: usize,
            warmup: usize,
            per_hash_mode: Option<PerModePerf>,
            skeleton_mode: Option<PerModePerf>,
        }
        fn build(
            counts: &HashMap<B256, [u64; PERF_NEVENTS + 1]>,
            walls: &HashMap<B256, u128>,
            event_set_name: &str,
            event_order: [&'static str; PERF_NEVENTS],
            is_default: bool,
        ) -> PerModePerf {
            let mut rows: Vec<PerfRow> = counts
                .iter()
                .map(|(h, c)| PerfRow {
                    code_hash: format!("0x{h:x}"),
                    cycles: c[0],
                    instructions: c[1],
                    e2: c[2],
                    e3: c[3],
                    e4: c[4],
                    e5: c[5],
                    calls: c[6],
                    wall_ns: walls.get(h).copied().unwrap_or(0),
                    l1i_misses: if is_default { Some(c[2]) } else { None },
                    itlb_misses: if is_default { Some(c[3]) } else { None },
                    branches: if is_default { Some(c[4]) } else { None },
                    branch_misses: if is_default { Some(c[5]) } else { None },
                })
                .collect();
            rows.sort_by(|a, b| a.code_hash.cmp(&b.code_hash));
            let total_calls = rows.iter().map(|r| r.calls).sum();
            let total_cycles = rows.iter().map(|r| r.cycles).sum();
            let total_instructions = rows.iter().map(|r| r.instructions).sum();
            let total_e2 = rows.iter().map(|r| r.e2).sum();
            let total_e3 = rows.iter().map(|r| r.e3).sum();
            let total_e4 = rows.iter().map(|r| r.e4).sum();
            let total_e5 = rows.iter().map(|r| r.e5).sum();
            let total_wall_ns = rows.iter().map(|r| r.wall_ns).sum();
            PerModePerf {
                event_set: event_set_name.to_string(),
                event_order,
                n_target_hashes: rows.len(),
                total_calls,
                total_cycles,
                total_instructions,
                total_e2,
                total_e3,
                total_e4,
                total_e5,
                total_wall_ns,
                total_l1i_misses: if is_default { Some(total_e2) } else { None },
                total_itlb_misses: if is_default { Some(total_e3) } else { None },
                total_branches: if is_default { Some(total_e4) } else { None },
                total_branch_misses: if is_default { Some(total_e5) } else { None },
                per_hash: rows,
            }
        }
        let event_set_name = match perf_set {
            PerfEventSet::Default => "default",
            PerfEventSet::Frontend => "frontend",
            PerfEventSet::Backend => "backend",
            PerfEventSet::TmaL1 => "tma_l1",
            PerfEventSet::TmaL2Bw => "tma_l2_bw",
            PerfEventSet::TmaL3Fetch => "tma_l3_fetch",
        };
        let event_order = perf_event_names();
        let is_default = matches!(perf_set, PerfEventSet::Default);
        let out = PerfOut {
            only_mode: cli.only_mode.clone(),
            blocks_sampled: blocks_measured,
            rounds: cli.rounds,
            warmup: cli.warmup,
            per_hash_mode: if mode_per_hash {
                Some(build(
                    &per_hash_perf,
                    &per_hash_wall_ns,
                    event_set_name,
                    event_order,
                    is_default,
                ))
            } else {
                None
            },
            skeleton_mode: if mode_skeleton {
                Some(build(
                    &skeleton_perf,
                    &skeleton_wall_ns,
                    event_set_name,
                    event_order,
                    is_default,
                ))
            } else {
                None
            },
        };
        std::fs::write(perf_out, serde_json::to_vec_pretty(&out).expect("serialize perf"))
            .expect("write perf out");
        println!("Wrote per-target-tx perf counters to {perf_out}");
    }

    if correctness_blocks_rejected > 0 {
        eprintln!(
            "\nCORRECTNESS GATE FAILED: {correctness_tx_mismatches} tx mismatches across \
             {correctness_blocks_rejected} blocks; those blocks are excluded from every arm."
        );
        std::process::exit(3);
    }

    println!("\nDone.");
}
