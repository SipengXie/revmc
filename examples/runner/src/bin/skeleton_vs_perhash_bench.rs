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
use std::time::{Duration, Instant};

use clap::Parser;
use op_revm::transaction::OpTransaction;
use op_revm::DefaultOp;
use revm::database::EmptyDB;
use revm::handler::{EvmTr, FrameResult, Handler, ItemOrResult};
use revm::primitives::B256;
use revmc::skeleton::{analyze_skeleton_group, build_data_table};
use revmc::OptimizationLevel;
use revmc_builtins as _;
use revmc_context::{EvmCompilerFn, RawEvmCompilerFn};
use serde::Serialize;

use bin_common::{build_op_cfg, build_op_tx, extract_gas, BenchError, BenchEvm, BinLoader, OpCtx};

const OPT: OptimizationLevel = OptimizationLevel::Aggressive;

#[derive(Parser)]
#[command(name = "skeleton_vs_perhash_bench")]
struct Cli {
    #[arg(long, default_value = "/home/ubuntu/sipeng/bench_data")]
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
}

#[derive(Serialize)]
struct Config {
    blocks_sampled: usize,
    rounds: usize,
    warmup: usize,
    alpha: f64,
}

#[derive(Serialize)]
struct Summary {
    total_family_members_measured: usize,
    skeleton_faster: usize,
    equivalent: usize,
    skeleton_slower: usize,
    median_speedup_ratio: f64,
    mean_delta_us: f64,
    p95_delta_us: f64,
}

#[derive(Serialize)]
struct ContractRecord {
    code_hash: String,
    skeleton_hash: String,
    family_size: usize,
    blocks_seen: usize,
    mean_per_hash_us: f64,
    mean_skeleton_us: f64,
    delta_us: f64,
    speedup_ratio: f64,
    welch_p: f64,
    verdict: String,
}

#[derive(Serialize)]
struct Output {
    config: Config,
    summary: Summary,
    per_contract: Vec<ContractRecord>,
}

fn welch_t(a: &[f64], b: &[f64]) -> (f64, f64) {
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    let var = |v: &[f64], m: f64| {
        v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (v.len() as f64 - 1.0).max(1.0)
    };
    let ma = mean(a);
    let mb = mean(b);
    let va = var(a, ma);
    let vb = var(b, mb);
    let se = ((va / a.len() as f64) + (vb / b.len() as f64)).sqrt();
    if se == 0.0 {
        return (0.0, 1.0);
    }
    let t = (ma - mb) / se;
    let z = t.abs();
    let p = 2.0 * (1.0 - 0.5 * (1.0 + libm::erf(z / std::f64::consts::SQRT_2)));
    (t, p)
}

fn load_skeleton_so(
    path: &Path,
    symbol: &str,
) -> Result<(RawEvmCompilerFn, libloading::Library), String> {
    let library = unsafe { libloading::Library::new(path) }.map_err(|e| format!("dlopen: {e}"))?;
    let sym = unsafe { library.get::<RawEvmCompilerFn>(symbol.as_bytes()) }
        .map_err(|e| format!("dlsym: {e}"))?;
    let function = *sym;
    drop(sym);
    Ok((function, library))
}

struct TimingPerHashHandler {
    functions: Arc<HashMap<B256, RawEvmCompilerFn>>,
    target_hashes: Arc<HashSet<B256>>,
    timings: HashMap<B256, Duration>,
}

impl Handler for TimingPerHashHandler {
    type Evm = BenchEvm;
    type Error = BenchError;
    type HaltReason = op_revm::OpHaltReason;

    fn run_exec_loop(
        &mut self,
        evm: &mut Self::Evm,
        first_frame_input: revm::interpreter::interpreter_action::FrameInit,
    ) -> Result<FrameResult, Self::Error> {
        let res = evm.frame_init(first_frame_input)?;
        if let ItemOrResult::Result(frame_result) = res {
            return Ok(frame_result);
        }
        loop {
            let call_or_result = self.run_frame(evm)?;
            let result = match call_or_result {
                ItemOrResult::Item(init) => match evm.frame_init(init)? {
                    ItemOrResult::Item(_) => continue,
                    ItemOrResult::Result(result) => result,
                },
                ItemOrResult::Result(result) => result,
            };
            if let Some(result) = evm.frame_return_result(result)? {
                return Ok(result);
            }
        }
    }
}

impl TimingPerHashHandler {
    fn run_frame(
        &mut self,
        evm: &mut BenchEvm,
    ) -> Result<
        ItemOrResult<revm::interpreter::interpreter_action::FrameInit, FrameResult>,
        BenchError,
    > {
        let (ctx, frame_stack) = (&mut evm.0.ctx, &mut evm.0.frame_stack);
        let frame = frame_stack.get();

        if !bin_common::should_lookup_jit(
            frame.data.is_create(),
            frame.interpreter.input.bytecode_address,
            frame.interpreter.bytecode.is_empty(),
        ) {
            drop((ctx, frame_stack));
            return Ok(evm.frame_run()?);
        }

        let bytecode_hash = frame.interpreter.bytecode.get_or_calculate_hash();
        let is_target = self.target_hashes.contains(&bytecode_hash);

        if let Some(&raw_fn) = self.functions.get(&bytecode_hash) {
            let f = EvmCompilerFn::new(raw_fn);
            let t0 = if is_target { Some(Instant::now()) } else { None };
            let action = unsafe { f.call_with_interpreter(&mut frame.interpreter, ctx) };
            if let Some(t0) = t0 {
                *self.timings.entry(bytecode_hash).or_default() += t0.elapsed();
            }
            let result = frame.process_next_action::<_, BenchError>(ctx, action).inspect(|i| {
                if i.is_result() {
                    frame.set_finished(true);
                }
            })?;
            Ok(result)
        } else {
            drop((ctx, frame_stack));
            Ok(evm.frame_run()?)
        }
    }
}

struct TimingSkeletonHandler {
    per_hash_fns: Arc<HashMap<B256, RawEvmCompilerFn>>,
    skeleton_dispatch: Arc<HashMap<B256, (EvmCompilerFn, Vec<u8>)>>,
    target_hashes: Arc<HashSet<B256>>,
    timings: HashMap<B256, Duration>,
}

impl Handler for TimingSkeletonHandler {
    type Evm = BenchEvm;
    type Error = BenchError;
    type HaltReason = op_revm::OpHaltReason;

    fn run_exec_loop(
        &mut self,
        evm: &mut Self::Evm,
        first_frame_input: revm::interpreter::interpreter_action::FrameInit,
    ) -> Result<FrameResult, Self::Error> {
        let res = evm.frame_init(first_frame_input)?;
        if let ItemOrResult::Result(frame_result) = res {
            return Ok(frame_result);
        }
        loop {
            let call_or_result = self.run_frame(evm)?;
            let result = match call_or_result {
                ItemOrResult::Item(init) => match evm.frame_init(init)? {
                    ItemOrResult::Item(_) => continue,
                    ItemOrResult::Result(result) => result,
                },
                ItemOrResult::Result(result) => result,
            };
            if let Some(result) = evm.frame_return_result(result)? {
                return Ok(result);
            }
        }
    }
}

impl TimingSkeletonHandler {
    fn run_frame(
        &mut self,
        evm: &mut BenchEvm,
    ) -> Result<
        ItemOrResult<revm::interpreter::interpreter_action::FrameInit, FrameResult>,
        BenchError,
    > {
        let (ctx, frame_stack) = (&mut evm.0.ctx, &mut evm.0.frame_stack);
        let frame = frame_stack.get();

        if !bin_common::should_lookup_jit(
            frame.data.is_create(),
            frame.interpreter.input.bytecode_address,
            frame.interpreter.bytecode.is_empty(),
        ) {
            drop((ctx, frame_stack));
            return Ok(evm.frame_run()?);
        }

        let bytecode_hash = frame.interpreter.bytecode.get_or_calculate_hash();
        let is_target = self.target_hashes.contains(&bytecode_hash);

        if let Some((skel_fn, data_table)) = self.skeleton_dispatch.get(&bytecode_hash) {
            let t0 = if is_target { Some(Instant::now()) } else { None };
            let action = unsafe {
                skel_fn.call_with_interpreter_data(&mut frame.interpreter, ctx, data_table.as_ptr())
            };
            if let Some(t0) = t0 {
                *self.timings.entry(bytecode_hash).or_default() += t0.elapsed();
            }
            let result = frame.process_next_action::<_, BenchError>(ctx, action).inspect(|i| {
                if i.is_result() {
                    frame.set_finished(true);
                }
            })?;
            Ok(result)
        } else if let Some(&raw_fn) = self.per_hash_fns.get(&bytecode_hash) {
            let f = EvmCompilerFn::new(raw_fn);
            let action = unsafe { f.call_with_interpreter(&mut frame.interpreter, ctx) };
            let result = frame.process_next_action::<_, BenchError>(ctx, action).inspect(|i| {
                if i.is_result() {
                    frame.set_finished(true);
                }
            })?;
            Ok(result)
        } else {
            drop((ctx, frame_stack));
            Ok(evm.frame_run()?)
        }
    }
}

fn execute_block_timed(
    loader: &BinLoader,
    chain_id: Option<u64>,
    mut run_tx: impl FnMut(&mut BenchEvm) -> (bool, u64),
) -> Vec<(bool, u64)> {
    let cfg = build_op_cfg(chain_id);
    let dummy_tx = OpTransaction::builder().build_fill();
    let mut evm: BenchEvm = {
        let ctx =
            OpCtx::<EmptyDB>::op().with_cfg(cfg).with_db(loader.build_cache_db()).with_tx(dummy_tx);
        op_revm::OpEvm::new(ctx, ())
    };
    let mut results = Vec::with_capacity(loader.tx_count());
    for tx_bin in loader.raw_txs() {
        if tx_bin.tx_type == 0x7e {
            results.push((true, 0));
            continue;
        }
        evm.0.ctx.tx = build_op_tx(tx_bin);
        results.push(run_tx(&mut evm));
    }
    results
}

fn main() {
    let cli = Cli::parse();
    let bench_dir = Path::new(&cli.bench_dir);
    let per_hash_cache = PathBuf::from(&cli.per_hash_cache);
    let skeleton_cache = PathBuf::from(&cli.skeleton_cache);

    let block_numbers: Vec<u64> = (0..cli.block_count)
        .step_by(cli.sample_every as usize)
        .map(|i| cli.start_block + i)
        .collect();

    println!("=== Skeleton vs Per-Hash Frame-Level Benchmark ===");
    println!(
        "Blocks: {} sampled ({} to {}, every {})",
        block_numbers.len(),
        block_numbers.first().unwrap_or(&0),
        block_numbers.last().unwrap_or(&0),
        cli.sample_every,
    );
    println!("Rounds: {} measured, {} warmup\n", cli.rounds, cli.warmup);

    // Phase 1: Load all per-hash .so files
    println!("--- Phase 1: Loading per-hash .so files ---");
    let t0 = Instant::now();
    let mut per_hash_fns: HashMap<B256, RawEvmCompilerFn> = HashMap::new();
    let mut per_hash_libs: Vec<libloading::Library> = Vec::new();

    let per_hash_entries: Vec<_> = std::fs::read_dir(&per_hash_cache)
        .expect("read per-hash cache dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map_or(false, |ext| ext == "so" || ext == "dylib"))
        .collect();

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

    // Phase 2: Reconstruct skeleton families from block bytecodes
    println!("\n--- Phase 2: Reconstructing skeleton families ---");
    let t1 = Instant::now();

    let mut all_codes: HashMap<B256, Vec<u8>> = HashMap::new();
    let mut blocks_loaded = 0usize;
    for &block in &block_numbers {
        match BinLoader::new(bench_dir, block) {
            Ok(loader) => {
                for (h, bc) in loader.code_values() {
                    if !bc.is_empty() {
                        all_codes.entry(*h).or_insert_with(|| bc.original_byte_slice().to_vec());
                    }
                }
                blocks_loaded += 1;
            }
            Err(e) => eprintln!("  WARN: skip block {block}: {e}"),
        }
    }
    println!("  Scanned {blocks_loaded} blocks, {} unique bytecodes", all_codes.len());

    let mut skeleton_groups: HashMap<u64, Vec<(B256, Vec<u8>)>> = HashMap::new();
    for (hash, bytes) in &all_codes {
        if skelot_common::should_skip_bytes(bytes) {
            continue;
        }
        let skel = skelot_common::extract_opcode_skeleton(bytes);
        let skel_hash = skelot_common::hash_bytes(&skel);
        skeleton_groups.entry(skel_hash).or_default().push((*hash, bytes.clone()));
    }

    let dup_groups: Vec<_> =
        skeleton_groups.into_iter().filter(|(_, members)| members.len() >= 2).collect();

    struct AnalyzedGroup {
        skel_hash: u64,
        members: Vec<(B256, Vec<u8>)>,
        variance: revmc::skeleton::SkeletonVariance,
    }

    let analyzed: Vec<AnalyzedGroup> = dup_groups
        .into_iter()
        .map(|(skel_hash, members)| {
            let refs: Vec<&[u8]> = members.iter().map(|(_, b)| b.as_slice()).collect();
            let variance = analyze_skeleton_group(&refs);
            AnalyzedGroup { skel_hash, members, variance }
        })
        .collect();

    println!("  {} skeleton families found in {:.2}s", analyzed.len(), t1.elapsed().as_secs_f64());

    // Phase 3: Load skeleton .so files and build dispatch tables
    println!("\n--- Phase 3: Loading skeleton .so files ---");
    let t2 = Instant::now();

    let mut skeleton_dispatch: HashMap<B256, (EvmCompilerFn, Vec<u8>)> = HashMap::new();
    let mut skeleton_libs: Vec<libloading::Library> = Vec::new();
    let mut skeleton_hash_map: HashMap<B256, (u64, usize)> = HashMap::new();

    for g in &analyzed {
        let arts = skelot_common::skeleton_paths(&skeleton_cache, g.skel_hash, OPT);
        match load_skeleton_so(&arts.library, &arts.symbol) {
            Ok((raw_fn, lib)) => {
                skeleton_libs.push(lib);
                let skel_fn = EvmCompilerFn::new(raw_fn);
                for (hash, bytes) in &g.members {
                    let table = build_data_table(bytes, &g.variance);
                    skeleton_dispatch.insert(*hash, (skel_fn, table.data));
                    skeleton_hash_map.insert(*hash, (g.skel_hash, g.members.len()));
                }
            }
            Err(e) => {
                eprintln!("  WARN: skeleton {:016x} load failed: {e}", g.skel_hash);
            }
        }
    }

    println!(
        "  {} skeleton-dispatched contracts in {:.2}s",
        skeleton_dispatch.len(),
        t2.elapsed().as_secs_f64()
    );

    // Phase 4: Identify treatment group
    let target_hashes: HashSet<B256> =
        skeleton_dispatch.keys().filter(|h| per_hash_fns.contains_key(*h)).copied().collect();

    println!(
        "\n--- Treatment group: {} contracts (in both per-hash and skeleton) ---",
        target_hashes.len()
    );

    if target_hashes.is_empty() {
        eprintln!("No contracts in treatment group. Nothing to compare.");
        std::process::exit(1);
    }

    let per_hash_fns = Arc::new(per_hash_fns);
    let skeleton_dispatch = Arc::new(skeleton_dispatch);
    let target_hashes = Arc::new(target_hashes);

    // Phase 5: Run blocks with timing
    println!("\n--- Phase 5: Measuring {} blocks ---", block_numbers.len());

    let mut per_hash_samples: HashMap<B256, Vec<f64>> = HashMap::new();
    let mut skeleton_samples: HashMap<B256, Vec<f64>> = HashMap::new();
    let mut blocks_measured = 0usize;

    for (bi, &block) in block_numbers.iter().enumerate() {
        let loader = match BinLoader::new(bench_dir, block) {
            Ok(l) => l,
            Err(_) => continue,
        };
        let chain_id = loader.raw_txs().first().and_then(|tx| tx.chain_id);

        // Per-hash warmup
        for _ in 0..cli.warmup {
            execute_block_timed(&loader, chain_id, |evm| {
                let mut h = TimingPerHashHandler {
                    functions: per_hash_fns.clone(),
                    target_hashes: target_hashes.clone(),
                    timings: HashMap::new(),
                };
                match h.run(evm) {
                    Ok(rr) => extract_gas(&rr),
                    Err(_) => (false, 0),
                }
            });
        }

        // Per-hash measured rounds
        for _ in 0..cli.rounds {
            let mut handler = TimingPerHashHandler {
                functions: per_hash_fns.clone(),
                target_hashes: target_hashes.clone(),
                timings: HashMap::new(),
            };
            execute_block_timed(&loader, chain_id, |evm| match handler.run(evm) {
                Ok(rr) => extract_gas(&rr),
                Err(_) => (false, 0),
            });
            for (hash, dur) in &handler.timings {
                per_hash_samples.entry(*hash).or_default().push(dur.as_nanos() as f64 / 1000.0);
            }
        }

        // Skeleton warmup
        for _ in 0..cli.warmup {
            execute_block_timed(&loader, chain_id, |evm| {
                let mut h = TimingSkeletonHandler {
                    per_hash_fns: per_hash_fns.clone(),
                    skeleton_dispatch: skeleton_dispatch.clone(),
                    target_hashes: target_hashes.clone(),
                    timings: HashMap::new(),
                };
                match h.run(evm) {
                    Ok(rr) => extract_gas(&rr),
                    Err(_) => (false, 0),
                }
            });
        }

        // Skeleton measured rounds
        for _ in 0..cli.rounds {
            let mut handler = TimingSkeletonHandler {
                per_hash_fns: per_hash_fns.clone(),
                skeleton_dispatch: skeleton_dispatch.clone(),
                target_hashes: target_hashes.clone(),
                timings: HashMap::new(),
            };
            execute_block_timed(&loader, chain_id, |evm| match handler.run(evm) {
                Ok(rr) => extract_gas(&rr),
                Err(_) => (false, 0),
            });
            for (hash, dur) in &handler.timings {
                skeleton_samples.entry(*hash).or_default().push(dur.as_nanos() as f64 / 1000.0);
            }
        }

        blocks_measured += 1;
        if (bi + 1) % 10 == 0 || bi + 1 == block_numbers.len() {
            println!("  [{}/{}] block {} done", bi + 1, block_numbers.len(), block);
        }
    }

    println!("\n  {blocks_measured} blocks measured");

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

    for hash in &all_measured {
        let ph = &per_hash_samples[hash];
        let sk = &skeleton_samples[hash];
        if ph.len() < 2 || sk.len() < 2 {
            continue;
        }

        let mean_ph = ph.iter().sum::<f64>() / ph.len() as f64;
        let mean_sk = sk.iter().sum::<f64>() / sk.len() as f64;
        let delta = mean_sk - mean_ph;
        let ratio = if mean_sk > 0.0 { mean_ph / mean_sk } else { 1.0 };

        let (_, p) = welch_t(ph, sk);

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

        all_ratios.push(ratio);
        all_deltas.push(delta);

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
            verdict: verdict.to_string(),
        });
    }

    records.sort_by(|a, b| {
        a.delta_us
            .abs()
            .partial_cmp(&b.delta_us.abs())
            .unwrap_or(std::cmp::Ordering::Equal)
            .reverse()
    });

    all_ratios.sort_by(|a, b| a.partial_cmp(b).unwrap());
    all_deltas.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let median_ratio = if all_ratios.is_empty() { 1.0 } else { all_ratios[all_ratios.len() / 2] };
    let mean_delta = if all_deltas.is_empty() {
        0.0
    } else {
        all_deltas.iter().sum::<f64>() / all_deltas.len() as f64
    };
    let p95_delta = if all_deltas.is_empty() {
        0.0
    } else {
        let idx = (all_deltas.len() as f64 * 0.95).ceil() as usize;
        all_deltas[idx.min(all_deltas.len() - 1)]
    };

    let total = records.len();
    println!("\n=== Results ({total} contracts measured) ===");
    println!("  SKELETON_FASTER:  {n_faster}");
    println!("  EQUIVALENT:       {n_equiv}");
    println!("  SKELETON_SLOWER:  {n_slower}");
    println!("  Median speedup ratio: {median_ratio:.4}");
    println!("  Mean delta (us):      {mean_delta:.2}");
    println!("  P95 delta (us):       {p95_delta:.2}");

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
            },
            per_contract: records,
        };
        std::fs::write(out_path, serde_json::to_vec_pretty(&output).expect("serialize"))
            .expect("write output");
        println!("\nWrote {out_path}");
    }

    println!("\nDone.");
}
