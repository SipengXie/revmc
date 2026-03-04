//! Per-contract JIT ablation experiment.
//!
//! For each transaction in a block, systematically disables one JIT-compiled
//! contract at a time and measures the performance impact. Uses Welch's t-test
//! to classify each contract as JIT-GOOD, JIT-BAD, or NEUTRAL.
//!
//! Usage:
//!   cargo run -p revmc-examples-runner --bin ablation_bench --release -- \
//!     --block 38004930 --cache-dir /tmp/jit_cache --rounds 15

#[allow(dead_code)]
#[path = "../bin_common.rs"]
mod bin_common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use op_revm::transaction::OpTransaction;
use op_revm::{DefaultOp, OpHaltReason};
use revm::{
    database::{CacheDB, EmptyDB},
    handler::{EvmTr, FrameResult, Handler, ItemOrResult},
    primitives::B256,
};
use revmc::OptimizationLevel;
use revmc_builtins as _;
use revmc_context::RawEvmCompilerFn;

use bin_common::{
    build_op_cfg, build_op_tx, compile_all_contracts_with_cache, run_jit_or_native, should_lookup_jit, BenchError,
    BenchEvm, BinLoader, JitHandler, NativeHandler, OpCtx, TxBin,
};

// ── CLI ─────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "ablation_bench", about = "Per-contract JIT ablation experiment")]
struct Args {
    /// Path to bench_data directory containing states/ and txs/
    #[arg(long, default_value = "/home/ubuntu/sipeng/bench_data")]
    dir: String,

    /// Block number to analyze
    #[arg(long)]
    block: u64,

    /// AOT cache directory (required)
    #[arg(long)]
    cache_dir: String,

    /// Number of timed measurement rounds per configuration
    #[arg(long, default_value_t = 15)]
    rounds: usize,

    /// Number of untimed warmup runs before measurement
    #[arg(long, default_value_t = 3)]
    warmup: usize,

    /// Significance level for Welch's t-test
    #[arg(long, default_value_t = 0.05)]
    alpha: f64,

    /// Print per-tx ablation details
    #[arg(long)]
    verbose: bool,
}

// ── Snapshot Collection ─────────────────────────────────────────────────────

/// Run the block sequentially, capturing CacheDB state before each non-deposit tx.
fn collect_snapshots(loader: &BinLoader) -> Vec<(usize, CacheDB<EmptyDB>)> {
    let chain_id = loader.raw_txs().first().and_then(|tx| tx.chain_id);
    let cfg = build_op_cfg(chain_id);
    let dummy_tx = OpTransaction::builder().build_fill();
    let mut evm: BenchEvm = {
        let ctx = OpCtx::<EmptyDB>::op()
            .with_cfg(cfg)
            .with_db(loader.build_cache_db())
            .with_tx(dummy_tx);
        op_revm::OpEvm::new(ctx, ())
    };

    let mut snapshots = Vec::new();
    for (i, tx_bin) in loader.raw_txs().iter().enumerate() {
        if tx_bin.tx_type == 0x7e {
            continue;
        }
        let snapshot_db = evm.0.ctx.journaled_state.database.clone();
        snapshots.push((i, snapshot_db));

        evm.0.ctx.tx = build_op_tx(tx_bin);
        let mut handler = NativeHandler;
        let _ = handler.run(&mut evm);
    }
    snapshots
}

// ── Discovery Handler ───────────────────────────────────────────────────────

/// Handler that records which JIT-compiled contracts are touched during execution.
struct DiscoveryHandler {
    all_functions: Arc<HashMap<B256, RawEvmCompilerFn>>,
    counts: HashMap<B256, usize>,
}

impl DiscoveryHandler {
    fn new(all_functions: Arc<HashMap<B256, RawEvmCompilerFn>>) -> Self {
        Self {
            all_functions,
            counts: HashMap::new(),
        }
    }

    fn into_counts(self) -> HashMap<B256, usize> {
        self.counts
    }
}

impl Handler for DiscoveryHandler {
    type Evm = BenchEvm;
    type Error = BenchError;
    type HaltReason = OpHaltReason;

    fn run_exec_loop(
        &mut self,
        evm: &mut Self::Evm,
        first_frame_input: revm::interpreter::interpreter_action::FrameInit,
    ) -> Result<FrameResult, Self::Error> {
        let res = evm.frame_init(first_frame_input)?;
        if let ItemOrResult::Result(frame_result) = res {
            return Ok(frame_result);
        }

        // Record the first frame
        {
            let frame = evm.0.frame_stack.get();
            if should_lookup_jit(
                frame.data.is_create(),
                frame.interpreter.input.bytecode_address,
                frame.interpreter.bytecode.is_empty(),
            ) {
                let hash = frame.interpreter.bytecode.get_or_calculate_hash();
                if self.all_functions.contains_key(&hash) {
                    *self.counts.entry(hash).or_insert(0) += 1;
                }
            }
        }

        loop {
            let call_or_result = run_jit_or_native(evm, &self.all_functions)?;
            let result = match call_or_result {
                ItemOrResult::Item(init) => match evm.frame_init(init)? {
                    ItemOrResult::Item(_) => {
                        // Record newly entered frame
                        let frame = evm.0.frame_stack.get();
                        if should_lookup_jit(
                            frame.data.is_create(),
                            frame.interpreter.input.bytecode_address,
                            frame.interpreter.bytecode.is_empty(),
                        ) {
                            let hash = frame.interpreter.bytecode.get_or_calculate_hash();
                            if self.all_functions.contains_key(&hash) {
                                *self.counts.entry(hash).or_insert(0) += 1;
                            }
                        }
                        continue;
                    }
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

// ── Ablation Data ───────────────────────────────────────────────────────────

struct TxAblationResult {
    tx_index: usize,
    jit_times: Vec<Duration>,
    native_times: Vec<Duration>,
    contract_results: Vec<ContractAblation>,
}

struct ContractAblation {
    code_hash: B256,
    frame_count: usize,
    ablated_times: Vec<Duration>,
}

/// Replay a single tx from a snapshot. Empty `functions` map = all native.
fn replay_tx(
    tx_bin: &TxBin,
    snapshot_db: &CacheDB<EmptyDB>,
    chain_id: Option<u64>,
    functions: &HashMap<B256, RawEvmCompilerFn>,
) -> Duration {
    let cfg = build_op_cfg(chain_id);
    let op_tx = build_op_tx(tx_bin);
    let dummy_tx = OpTransaction::builder().build_fill();
    let mut evm: BenchEvm = {
        let ctx = OpCtx::<EmptyDB>::op()
            .with_cfg(cfg)
            .with_db(snapshot_db.clone())
            .with_tx(dummy_tx);
        op_revm::OpEvm::new(ctx, ())
    };
    evm.0.ctx.tx = op_tx;

    if functions.is_empty() {
        let mut handler = NativeHandler;
        let t0 = Instant::now();
        let _ = handler.run(&mut evm);
        t0.elapsed()
    } else {
        let mut handler = JitHandler {
            functions: Arc::new(functions.clone()),
        };
        let t0 = Instant::now();
        let _ = handler.run(&mut evm);
        t0.elapsed()
    }
}

/// Discover which JIT-compiled contracts are touched by this tx.
/// Returns map of code_hash -> frame_count.
fn discover_contracts(
    tx_bin: &TxBin,
    snapshot_db: &CacheDB<EmptyDB>,
    chain_id: Option<u64>,
    all_functions: &Arc<HashMap<B256, RawEvmCompilerFn>>,
) -> HashMap<B256, usize> {
    let cfg = build_op_cfg(chain_id);
    let op_tx = build_op_tx(tx_bin);
    let dummy_tx = OpTransaction::builder().build_fill();
    let mut evm: BenchEvm = {
        let ctx = OpCtx::<EmptyDB>::op()
            .with_cfg(cfg)
            .with_db(snapshot_db.clone())
            .with_tx(dummy_tx);
        op_revm::OpEvm::new(ctx, ())
    };
    evm.0.ctx.tx = op_tx;

    let mut handler = DiscoveryHandler::new(all_functions.clone());
    let _ = handler.run(&mut evm);
    handler.into_counts()
}

// ── Statistics (zero-dependency Welch's t-test) ─────────────────────────────

struct WelchResult {
    mean_diff: f64,
    p_value: f64,
}

fn ln_gamma(x: f64) -> f64 {
    const G: f64 = 7.0;
    const COEFF: [f64; 9] = [
        0.99999999999980993,
        676.5203681218851,
        -1259.1392167224028,
        771.32342877765313,
        -176.61502916214059,
        12.507343278686905,
        -0.13857109526572012,
        9.9843695780195716e-6,
        1.5056327351493116e-7,
    ];
    let x = x - 1.0;
    let mut sum = COEFF[0];
    for (i, &c) in COEFF[1..].iter().enumerate() {
        sum += c / (x + i as f64 + 1.0);
    }
    let t = x + G + 0.5;
    0.5 * (2.0 * std::f64::consts::PI).ln() + (t.ln() * (x + 0.5)) - t + sum.ln()
}

fn regularized_incomplete_beta(a: f64, b: f64, x: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    if x > (a + 1.0) / (a + b + 2.0) {
        return 1.0 - regularized_incomplete_beta(b, a, 1.0 - x);
    }
    let log_prefix =
        a * x.ln() + b * (1.0 - x).ln() - ln_gamma(a) - ln_gamma(b) + ln_gamma(a + b) - a.ln();
    const TINY: f64 = 1e-30;
    const EPS: f64 = 1e-14;
    let mut c = 1.0;
    let mut d = 1.0 - (a + b) * x / (a + 1.0);
    if d.abs() < TINY {
        d = TINY;
    }
    d = 1.0 / d;
    let mut result = d;
    for m in 1..=200 {
        let m_f = m as f64;
        let num_even = m_f * (b - m_f) * x / ((a + 2.0 * m_f - 1.0) * (a + 2.0 * m_f));
        d = 1.0 + num_even * d;
        if d.abs() < TINY {
            d = TINY;
        }
        c = 1.0 + num_even / c;
        if c.abs() < TINY {
            c = TINY;
        }
        d = 1.0 / d;
        result *= c * d;
        let num_odd =
            -(a + m_f) * (a + b + m_f) * x / ((a + 2.0 * m_f) * (a + 2.0 * m_f + 1.0));
        d = 1.0 + num_odd * d;
        if d.abs() < TINY {
            d = TINY;
        }
        c = 1.0 + num_odd / c;
        if c.abs() < TINY {
            c = TINY;
        }
        d = 1.0 / d;
        let delta = c * d;
        result *= delta;
        if (delta - 1.0).abs() < EPS {
            break;
        }
    }
    log_prefix.exp() * result
}

fn t_cdf(t: f64, df: f64) -> f64 {
    let x = df / (df + t * t);
    let ib = regularized_incomplete_beta(df / 2.0, 0.5, x);
    if t >= 0.0 {
        1.0 - 0.5 * ib
    } else {
        0.5 * ib
    }
}

fn welch_t_test(a: &[f64], b: &[f64]) -> Option<WelchResult> {
    let (n_a, n_b) = (a.len(), b.len());
    if n_a < 2 || n_b < 2 {
        return None;
    }
    let mean_a = a.iter().sum::<f64>() / n_a as f64;
    let mean_b = b.iter().sum::<f64>() / n_b as f64;
    let var_a = a.iter().map(|x| (x - mean_a).powi(2)).sum::<f64>() / (n_a - 1) as f64;
    let var_b = b.iter().map(|x| (x - mean_b).powi(2)).sum::<f64>() / (n_b - 1) as f64;
    let (n_a_f, n_b_f) = (n_a as f64, n_b as f64);
    let se_sq = var_a / n_a_f + var_b / n_b_f;
    if se_sq == 0.0 {
        let md = mean_a - mean_b;
        return Some(WelchResult {
            mean_diff: md,
            p_value: if md == 0.0 { 1.0 } else { 0.0 },
        });
    }
    let t_stat = (mean_a - mean_b) / se_sq.sqrt();
    let df = se_sq.powi(2)
        / (var_a.powi(2) / (n_a_f.powi(2) * (n_a_f - 1.0))
            + var_b.powi(2) / (n_b_f.powi(2) * (n_b_f - 1.0)));
    let p_value = 2.0 * (1.0 - t_cdf(t_stat.abs(), df));
    Some(WelchResult {
        mean_diff: mean_a - mean_b,
        p_value,
    })
}

fn cohen_d(a: &[f64], b: &[f64]) -> f64 {
    let mean_a = a.iter().sum::<f64>() / a.len() as f64;
    let mean_b = b.iter().sum::<f64>() / b.len() as f64;
    let var_a = a.iter().map(|x| (x - mean_a).powi(2)).sum::<f64>() / (a.len() - 1) as f64;
    let var_b = b.iter().map(|x| (x - mean_b).powi(2)).sum::<f64>() / (b.len() - 1) as f64;
    let pooled_sd = ((var_a + var_b) / 2.0).sqrt();
    if pooled_sd == 0.0 {
        0.0
    } else {
        (mean_a - mean_b) / pooled_sd
    }
}

fn median_us(v: &[Duration]) -> f64 {
    let mut us: Vec<f64> = v.iter().map(|d| d.as_secs_f64() * 1e6).collect();
    us.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if us.len() % 2 == 0 {
        (us[us.len() / 2 - 1] + us[us.len() / 2]) / 2.0
    } else {
        us[us.len() / 2]
    }
}

fn to_us(v: &[Duration]) -> Vec<f64> {
    v.iter().map(|d| d.as_secs_f64() * 1e6).collect()
}

fn verdict(p_value: f64, mean_diff: f64, alpha: f64) -> &'static str {
    if p_value >= alpha {
        "NEUTRAL"
    } else if mean_diff > 0.0 {
        "JIT-GOOD"
    } else {
        "JIT-BAD"
    }
}

// ── Main ────────────────────────────────────────────────────────────────────

fn main() {
    let args = Args::parse();
    let bench_dir = Path::new(&args.dir);

    eprintln!("=== JIT Ablation: block {} ===", args.block);
    eprintln!(
        "  rounds={}, warmup={}, alpha={}",
        args.rounds, args.warmup, args.alpha
    );

    // Phase 0: Load data
    eprintln!("\n=== Phase 0: Load block data ===");
    let loader = BinLoader::new(bench_dir, args.block).expect("load block");
    eprintln!(
        "  {} txs, {} accounts, {} codes",
        loader.tx_count(),
        loader.account_count(),
        loader.code_count()
    );

    // Phase 1: Compile contracts
    eprintln!("\n=== Phase 1: Load JIT cache ===");
    let all_codes: HashMap<B256, _> = loader
        .code_values()
        .iter()
        .map(|(h, b)| (*h, b.clone()))
        .collect();
    let compiled = compile_all_contracts_with_cache(
        &all_codes,
        OptimizationLevel::Aggressive,
        Path::new(&args.cache_dir),
    );
    let all_functions = compiled.functions;

    // Phase 2: Collect per-tx snapshots
    eprintln!("\n=== Phase 2: Collect per-tx snapshots ===");
    let snapshots = collect_snapshots(&loader);
    eprintln!("  Collected {} snapshots", snapshots.len());

    // Phase 3: Run ablation
    eprintln!("\n=== Phase 3: Per-tx ablation ===");
    let chain_id = loader.raw_txs().first().and_then(|tx| tx.chain_id);
    let all_fns_map: HashMap<B256, RawEvmCompilerFn> =
        all_functions.iter().map(|(&h, &f)| (h, f)).collect();
    let empty_fns: HashMap<B256, RawEvmCompilerFn> = HashMap::new();

    let mut tx_results: Vec<TxAblationResult> = Vec::new();

    for (tx_index, snapshot_db) in &snapshots {
        let tx_bin = &loader.raw_txs()[*tx_index];

        let contract_counts = discover_contracts(tx_bin, snapshot_db, chain_id, &all_functions);
        if contract_counts.is_empty() {
            continue;
        }

        let unique_contracts: Vec<(B256, usize)> = contract_counts.into_iter().collect();
        eprintln!(
            "  tx[{}]: {} JIT contracts",
            tx_index,
            unique_contracts.len()
        );

        // Warmup + Baseline: full JIT
        for _ in 0..args.warmup {
            replay_tx(tx_bin, snapshot_db, chain_id, &all_fns_map);
        }
        let jit_times: Vec<Duration> = (0..args.rounds)
            .map(|_| replay_tx(tx_bin, snapshot_db, chain_id, &all_fns_map))
            .collect();

        // Warmup + Baseline: full native
        for _ in 0..args.warmup {
            replay_tx(tx_bin, snapshot_db, chain_id, &empty_fns);
        }
        let native_times: Vec<Duration> = (0..args.rounds)
            .map(|_| replay_tx(tx_bin, snapshot_db, chain_id, &empty_fns))
            .collect();

        // Ablate each contract
        let mut contract_results = Vec::new();
        for &(code_hash, frame_count) in &unique_contracts {
            let ablated_fns: HashMap<B256, RawEvmCompilerFn> = all_fns_map
                .iter()
                .filter(|(&h, _)| h != code_hash)
                .map(|(&h, &f)| (h, f))
                .collect();

            for _ in 0..args.warmup {
                replay_tx(tx_bin, snapshot_db, chain_id, &ablated_fns);
            }
            let ablated_times: Vec<Duration> = (0..args.rounds)
                .map(|_| replay_tx(tx_bin, snapshot_db, chain_id, &ablated_fns))
                .collect();

            contract_results.push(ContractAblation {
                code_hash,
                frame_count,
                ablated_times,
            });
        }

        tx_results.push(TxAblationResult {
            tx_index: *tx_index,
            jit_times,
            native_times,
            contract_results,
        });
    }

    // Phase 4: Report
    eprintln!("\n=== Phase 4: Block summary ===\n");

    struct AggEntry {
        appearances: usize,
        total_frames: usize,
        deltas_us: Vec<f64>,
    }
    let mut agg: HashMap<B256, AggEntry> = HashMap::new();

    for txr in &tx_results {
        let jit_med = median_us(&txr.jit_times);
        let native_med = median_us(&txr.native_times);

        if args.verbose {
            println!(
                "tx[{}]: {} contracts, jit_median={:.1}us, native_median={:.1}us",
                txr.tx_index,
                txr.contract_results.len(),
                jit_med,
                native_med,
            );
            println!(
                "  {:>16}  {:>6}  {:>10}  {:>10}  {:>8}  {:>8}  {}",
                "Contract", "Frames", "Ablat(us)", "Delta(us)", "p-value", "Cohen_d", "Verdict"
            );
        }

        let jit_us = to_us(&txr.jit_times);
        for cr in &txr.contract_results {
            let ablated_us = to_us(&cr.ablated_times);
            let ablat_med = median_us(&cr.ablated_times);

            let (mean_diff, p_val, d) = if let Some(wr) = welch_t_test(&ablated_us, &jit_us) {
                let d = cohen_d(&ablated_us, &jit_us);
                (wr.mean_diff, wr.p_value, d)
            } else {
                (0.0, 1.0, 0.0)
            };

            let v = verdict(p_val, mean_diff, args.alpha);

            if args.verbose {
                let short = &hex::encode(cr.code_hash)[..12];
                println!(
                    "  {short:>16}  {:>6}  {:>10.1}  {:>+10.1}  {:>8.4}  {:>8.2}  {v}",
                    cr.frame_count, ablat_med, mean_diff, p_val, d.abs(),
                );
            }

            let entry = agg.entry(cr.code_hash).or_insert(AggEntry {
                appearances: 0,
                total_frames: 0,
                deltas_us: Vec::new(),
            });
            entry.appearances += 1;
            entry.total_frames += cr.frame_count;
            entry.deltas_us.push(mean_diff);
        }

        if args.verbose {
            println!();
        }
    }

    // Block-level summary
    println!("=== Block Summary ({}) ===\n", args.block);
    println!(
        "{:>16}  {:>8}  {:>8}  {:>13}  {:>8}  {}",
        "Contract", "Appears", "Frames", "Avg_Delta(us)", "Agg_p", "Verdict"
    );

    let mut entries: Vec<_> = agg.iter().collect();
    entries.sort_by(|a, b| {
        b.1.deltas_us
            .iter()
            .sum::<f64>()
            .partial_cmp(&a.1.deltas_us.iter().sum::<f64>())
            .unwrap()
    });

    let (mut n_good, mut n_bad, mut n_neutral) = (0usize, 0usize, 0usize);
    let mut good_contracts: Vec<B256> = Vec::new();

    for (hash, entry) in &entries {
        let avg_delta = entry.deltas_us.iter().sum::<f64>() / entry.deltas_us.len() as f64;

        let zeros: Vec<f64> = vec![0.0; entry.deltas_us.len()];
        let (agg_p, v) = if entry.deltas_us.len() >= 2 {
            if let Some(wr) = welch_t_test(&entry.deltas_us, &zeros) {
                (wr.p_value, verdict(wr.p_value, wr.mean_diff, args.alpha))
            } else {
                (1.0, "NEUTRAL")
            }
        } else {
            let single_p = if entry.deltas_us[0].abs() > 1.0 {
                0.01
            } else {
                1.0
            };
            (single_p, verdict(single_p, entry.deltas_us[0], args.alpha))
        };

        match v {
            "JIT-GOOD" => {
                n_good += 1;
                good_contracts.push(**hash);
            }
            "JIT-BAD" => n_bad += 1,
            _ => n_neutral += 1,
        }

        let short = &hex::encode(hash)[..12];
        println!(
            "{short:>16}  {:>8}  {:>8}  {:>+13.1}  {:>8.4}  {v}",
            entry.appearances, entry.total_frames, avg_delta, agg_p,
        );
    }

    println!(
        "\nJIT-GOOD: {n_good} | JIT-BAD: {n_bad} | NEUTRAL: {n_neutral}"
    );
    println!(
        "Optimal whitelist: {} contracts (JIT-GOOD only)",
        good_contracts.len()
    );
}
