//! Full-block JIT ablation: generate whitelist under realistic L1i pressure.
//!
//! Unlike ablation_bench (isolated per-tx snapshots), this tool replays the
//! entire block for each ablation config, capturing L1i cache effects.
//!
//! Usage:
//!   cargo run -p revmc-examples-runner --bin fullblock_ablation --release -- \
//!     --block 38004930 --cache-dir /tmp/jit_cache --rounds 15 --warmup 3 \
//!     --output /tmp/fullblock_whitelist.json

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
    database::EmptyDB,
    handler::{EvmTr, FrameResult, Handler, ItemOrResult},
    primitives::B256,
};
use revmc::OptimizationLevel;
use revmc_builtins as _;
use revmc_context::RawEvmCompilerFn;

use bin_common::{
    build_op_cfg, build_op_tx, compile_all_contracts_with_cache, run_jit_or_native,
    should_lookup_jit, BenchError, BenchEvm, BinLoader, JitHandler, NativeHandler, OpCtx,
};

// ── CLI ─────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "fullblock_ablation", about = "Full-block JIT ablation for whitelist generation")]
struct Args {
    #[arg(long, default_value = "/home/ubuntu/sipeng/bench_data")]
    dir: String,
    #[arg(long)]
    block: u64,
    #[arg(long)]
    cache_dir: String,
    #[arg(long, default_value_t = 15)]
    rounds: usize,
    #[arg(long, default_value_t = 3)]
    warmup: usize,
    #[arg(long, default_value_t = 0.05)]
    alpha: f64,
    /// Path to write whitelist JSON
    #[arg(long)]
    output: Option<String>,
    /// Benchmark mode: load whitelist JSON and compare native vs all-JIT vs selective
    #[arg(long)]
    benchmark: Option<String>,
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

// ── Full-Block Replay ───────────────────────────────────────────────────────

/// Replay full block, return per-tx durations (deposit txs get Duration::ZERO).
fn run_full_block(
    loader: &BinLoader,
    functions: Option<&Arc<HashMap<B256, RawEvmCompilerFn>>>,
) -> Vec<Duration> {
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

    let mut per_tx = Vec::with_capacity(loader.tx_count());
    for tx_bin in loader.raw_txs() {
        if tx_bin.tx_type == 0x7e {
            per_tx.push(Duration::ZERO);
            continue;
        }
        evm.0.ctx.tx = build_op_tx(tx_bin);
        let t0 = Instant::now();
        if let Some(fns) = functions {
            let mut h = JitHandler { functions: fns.clone() };
            let _ = h.run(&mut evm);
        } else {
            let mut h = NativeHandler;
            let _ = h.run(&mut evm);
        }
        per_tx.push(t0.elapsed());
    }
    per_tx
}

/// Run full block with discovery, return per-tx contract sets.
fn discover_full_block(
    loader: &BinLoader,
    all_functions: &Arc<HashMap<B256, RawEvmCompilerFn>>,
) -> Vec<Vec<(B256, usize)>> {
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

    let mut per_tx_contracts = Vec::with_capacity(loader.tx_count());
    for tx_bin in loader.raw_txs() {
        if tx_bin.tx_type == 0x7e {
            per_tx_contracts.push(Vec::new());
            continue;
        }
        evm.0.ctx.tx = build_op_tx(tx_bin);
        let mut handler = DiscoveryHandler::new(all_functions.clone());
        let _ = handler.run(&mut evm);
        let counts: Vec<(B256, usize)> = handler.into_counts().into_iter().collect();
        per_tx_contracts.push(counts);
    }
    per_tx_contracts
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

fn median_us(v: &[Duration]) -> f64 {
    let mut us: Vec<f64> = v.iter().map(|d| d.as_secs_f64() * 1e6).collect();
    us.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if us.is_empty() {
        return 0.0;
    }
    us[us.len() / 2]
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

// ── Benchmark Mode ──────────────────────────────────────────────────────────

fn benchmark_whitelist(
    loader: &BinLoader,
    all_functions: &Arc<HashMap<B256, RawEvmCompilerFn>>,
    whitelist_path: &str,
    args: &Args,
) {
    // Load whitelist JSON
    let json_str = std::fs::read_to_string(whitelist_path)
        .unwrap_or_else(|e| panic!("read {whitelist_path}: {e}"));
    let json: serde_json::Value =
        serde_json::from_str(&json_str).expect("parse whitelist JSON");
    let wl_hashes: Vec<B256> = json["whitelist"]
        .as_array()
        .expect("whitelist array")
        .iter()
        .filter_map(|v| {
            let s = v.as_str()?;
            let bytes = hex::decode(s.strip_prefix("0x").unwrap_or(s)).ok()?;
            Some(B256::from_slice(&bytes))
        })
        .collect();

    let parse_hashes = |key: &str| -> Vec<B256> {
        json[key]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|v| {
                let s = v.as_str()?;
                let bytes = hex::decode(s.strip_prefix("0x").unwrap_or(s)).ok()?;
                Some(B256::from_slice(&bytes))
            })
            .collect()
    };
    let bl_hashes: Vec<B256> = parse_hashes("blacklist");

    // Build function maps
    let good_only: Arc<HashMap<B256, RawEvmCompilerFn>> = Arc::new(
        all_functions
            .iter()
            .filter(|(h, _)| wl_hashes.contains(h))
            .map(|(&h, &f)| (h, f))
            .collect(),
    );
    let no_bad: Arc<HashMap<B256, RawEvmCompilerFn>> = Arc::new(
        all_functions
            .iter()
            .filter(|(h, _)| !bl_hashes.contains(h))
            .map(|(&h, &f)| (h, f))
            .collect(),
    );

    eprintln!(
        "\n=== Benchmark: good_only={} / no_bad={} / all={} ===",
        good_only.len(),
        no_bad.len(),
        all_functions.len()
    );

    // Warmup all four modes
    for _ in 0..args.warmup {
        run_full_block(loader, None);
        run_full_block(loader, Some(all_functions));
        run_full_block(loader, Some(&good_only));
        run_full_block(loader, Some(&no_bad));
    }

    // Timed rounds
    let mut native_times = Vec::with_capacity(args.rounds);
    let mut alljit_times = Vec::with_capacity(args.rounds);
    let mut good_only_times = Vec::with_capacity(args.rounds);
    let mut no_bad_times = Vec::with_capacity(args.rounds);

    for r in 0..args.rounds {
        let sum_us = |v: &[Duration]| -> f64 {
            v.iter().map(|d| d.as_secs_f64() * 1e6).sum()
        };

        native_times.push(sum_us(&run_full_block(loader, None)));
        alljit_times.push(sum_us(&run_full_block(loader, Some(all_functions))));
        good_only_times.push(sum_us(&run_full_block(loader, Some(&good_only))));
        no_bad_times.push(sum_us(&run_full_block(loader, Some(&no_bad))));

        if (r + 1) % 5 == 0 {
            eprintln!("  round {}/{}", r + 1, args.rounds);
        }
    }

    // Compute medians
    let med = |v: &mut Vec<f64>| -> f64 {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };

    let native_med = med(&mut native_times);
    let alljit_med = med(&mut alljit_times);
    let good_only_med = med(&mut good_only_times);
    let no_bad_med = med(&mut no_bad_times);

    println!("\n=== Full-Block Benchmark: block {} ===\n", args.block);
    println!(
        "{:>16}  {:>8}  {:>10}  {:>9}",
        "Mode", "JIT_Fns", "Time(ms)", "vs Native"
    );
    let print_row = |name: &str, fns: usize, ms: f64, baseline: f64| {
        println!(
            "{name:>16}  {fns:>8}  {ms:>10.2}  {speedup:>8.2}x",
            speedup = baseline / ms
        );
    };
    println!(
        "{:>16}  {:>8}  {:>10.2}  {:>9}",
        "Native", 0, native_med / 1000.0, "1.00x"
    );
    print_row("All-JIT", all_functions.len(), alljit_med / 1000.0, native_med / 1000.0);
    print_row("No-Bad", no_bad.len(), no_bad_med / 1000.0, native_med / 1000.0);
    print_row("Good-Only", good_only.len(), good_only_med / 1000.0, native_med / 1000.0);

    // Per-round details to stderr
    eprintln!("\nPer-round (ms): native / all-jit / no-bad / good-only");
    for r in 0..args.rounds {
        eprintln!(
            "  R{:02}: {:.2} / {:.2} / {:.2} / {:.2}",
            r,
            native_times[r] / 1000.0,
            alljit_times[r] / 1000.0,
            no_bad_times[r] / 1000.0,
            good_only_times[r] / 1000.0,
        );
    }
}

// ── Main ────────────────────────────────────────────────────────────────────

fn main() {
    let args = Args::parse();
    let loader = BinLoader::new(Path::new(&args.dir), args.block).expect("load block");

    eprintln!("=== Full-Block Ablation: block {} ===", args.block);
    eprintln!(
        "  rounds={}, warmup={}, alpha={}",
        args.rounds, args.warmup, args.alpha
    );

    // Load JIT
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
    eprintln!("  {} JIT functions loaded", all_functions.len());

    // ── Benchmark mode: load whitelist and compare ──────────────────────────
    if let Some(ref whitelist_path) = args.benchmark {
        benchmark_whitelist(&loader, &all_functions, whitelist_path, &args);
        return;
    }

    // ── Phase 0: Baseline ───────────────────────────────────────────────────
    eprintln!("\n=== Phase 0: Baseline ===");
    for _ in 0..args.warmup {
        run_full_block(&loader, None);
        run_full_block(&loader, Some(&all_functions));
    }
    let mut native_samples: Vec<Vec<Duration>> = Vec::with_capacity(args.rounds);
    let mut jit_samples: Vec<Vec<Duration>> = Vec::with_capacity(args.rounds);
    for r in 0..args.rounds {
        native_samples.push(run_full_block(&loader, None));
        jit_samples.push(run_full_block(&loader, Some(&all_functions)));
        if (r + 1) % 5 == 0 {
            eprintln!("  baseline round {}/{}", r + 1, args.rounds);
        }
    }

    let n_tx = loader.tx_count();
    let native_medians: Vec<f64> = (0..n_tx)
        .map(|i| median_us(&native_samples.iter().map(|s| s[i]).collect::<Vec<_>>()))
        .collect();
    let jit_medians: Vec<f64> = (0..n_tx)
        .map(|i| median_us(&jit_samples.iter().map(|s| s[i]).collect::<Vec<_>>()))
        .collect();
    let native_total: f64 = native_medians.iter().sum();
    let jit_total: f64 = jit_medians.iter().sum();
    eprintln!(
        "  native={:.1}ms jit={:.1}ms ({:.2}x)",
        native_total / 1000.0,
        jit_total / 1000.0,
        native_total / jit_total
    );

    // ── Phase 1: Discovery ──────────────────────────────────────────────────
    eprintln!("\n=== Phase 1: Discovery ===");
    let per_tx_contracts = discover_full_block(&loader, &all_functions);

    let mut contract_to_txs: HashMap<B256, Vec<usize>> = HashMap::new();
    let mut contract_frames: HashMap<B256, usize> = HashMap::new();
    for (i, contracts) in per_tx_contracts.iter().enumerate() {
        for &(hash, frame_count) in contracts {
            contract_to_txs.entry(hash).or_default().push(i);
            *contract_frames.entry(hash).or_insert(0) += frame_count;
        }
    }
    let unique_contracts: Vec<B256> = contract_to_txs.keys().copied().collect();
    eprintln!(
        "  {} unique JIT contracts across {} txs",
        unique_contracts.len(),
        per_tx_contracts.iter().filter(|c| !c.is_empty()).count()
    );

    // ── Phase 2: Per-contract ablation ──────────────────────────────────────
    eprintln!(
        "\n=== Phase 2: Per-contract ablation ({} contracts) ===",
        unique_contracts.len()
    );

    struct ContractResult {
        hash: B256,
        tx_count: usize,
        total_frames: usize,
        avg_delta_us: f64,
        p_value: f64,
        verdict: &'static str,
    }

    let mut results: Vec<ContractResult> = Vec::new();

    for (ci, &contract_hash) in unique_contracts.iter().enumerate() {
        // Build ablated function map (exclude this contract)
        let ablated_fns: Arc<HashMap<B256, RawEvmCompilerFn>> = Arc::new(
            all_functions
                .iter()
                .filter(|(&h, _)| h != contract_hash)
                .map(|(&h, &f)| (h, f))
                .collect(),
        );

        // Warmup
        for _ in 0..args.warmup {
            run_full_block(&loader, Some(&ablated_fns));
        }

        // Timed rounds
        let mut ablated_samples: Vec<Vec<Duration>> = Vec::with_capacity(args.rounds);
        for _ in 0..args.rounds {
            ablated_samples.push(run_full_block(&loader, Some(&ablated_fns)));
        }

        // Compare ablated vs all-JIT for each tx that touches this contract
        let affected_txs = &contract_to_txs[&contract_hash];
        let frames = contract_frames[&contract_hash];
        let mut deltas: Vec<f64> = Vec::new();

        for &tx_idx in affected_txs {
            let jit_us: Vec<f64> = jit_samples
                .iter()
                .map(|s| s[tx_idx].as_secs_f64() * 1e6)
                .collect();
            let abl_us: Vec<f64> = ablated_samples
                .iter()
                .map(|s| s[tx_idx].as_secs_f64() * 1e6)
                .collect();
            // delta = ablated - jit: positive means removing JIT made it slower (JIT helps)
            let jit_med = jit_us.iter().copied().sum::<f64>() / jit_us.len() as f64;
            let abl_med = abl_us.iter().copied().sum::<f64>() / abl_us.len() as f64;
            deltas.push(abl_med - jit_med);
        }

        // Aggregate: t-test on deltas vs zero
        let avg_delta = deltas.iter().sum::<f64>() / deltas.len().max(1) as f64;
        let (p_val, v) = if deltas.len() >= 2 {
            let zeros = vec![0.0f64; deltas.len()];
            if let Some(wr) = welch_t_test(&deltas, &zeros) {
                (wr.p_value, verdict(wr.p_value, wr.mean_diff, args.alpha))
            } else {
                (1.0, "NEUTRAL")
            }
        } else {
            let p = if deltas.first().map_or(false, |d| d.abs() > 1.0) {
                0.01
            } else {
                1.0
            };
            (p, verdict(p, deltas.first().copied().unwrap_or(0.0), args.alpha))
        };

        results.push(ContractResult {
            hash: contract_hash,
            tx_count: affected_txs.len(),
            total_frames: frames,
            avg_delta_us: avg_delta,
            p_value: p_val,
            verdict: v,
        });

        if (ci + 1) % 25 == 0 || ci + 1 == unique_contracts.len() {
            eprintln!("  ablated {}/{} contracts", ci + 1, unique_contracts.len());
        }
    }

    // ── Phase 3: Report ─────────────────────────────────────────────────────
    results.sort_by(|a, b| b.avg_delta_us.partial_cmp(&a.avg_delta_us).unwrap());

    println!("\n=== Full-Block Ablation: block {} ===\n", args.block);
    println!(
        "{:>16}  {:>5}  {:>6}  {:>13}  {:>8}  {}",
        "Contract", "Txs", "Frames", "Avg_Delta(us)", "p_value", "Verdict"
    );

    let mut n_good = 0usize;
    let mut n_bad = 0usize;
    let mut n_neutral = 0usize;
    let mut whitelist: Vec<String> = Vec::new();
    let mut blacklist: Vec<String> = Vec::new();

    for r in &results {
        let short = &hex::encode(r.hash)[..12];
        println!(
            "{short:>16}  {:>5}  {:>6}  {:>+13.1}  {:>8.4}  {}",
            r.tx_count, r.total_frames, r.avg_delta_us, r.p_value, r.verdict
        );
        match r.verdict {
            "JIT-GOOD" => {
                n_good += 1;
                whitelist.push(format!("0x{}", hex::encode(r.hash)));
            }
            "JIT-BAD" => {
                n_bad += 1;
                blacklist.push(format!("0x{}", hex::encode(r.hash)));
            }
            _ => n_neutral += 1,
        }
    }

    println!(
        "\nJIT-GOOD: {n_good} | JIT-BAD: {n_bad} | NEUTRAL: {n_neutral}"
    );
    println!("Whitelist: {} contracts (JIT-GOOD only)", whitelist.len());

    // JSON output
    if let Some(output_path) = &args.output {
        let json = serde_json::json!({
            "block": args.block,
            "rounds": args.rounds,
            "alpha": args.alpha,
            "whitelist": whitelist,
            "blacklist": blacklist,
            "summary": {
                "good": n_good,
                "bad": n_bad,
                "neutral": n_neutral
            }
        });
        std::fs::write(output_path, serde_json::to_string_pretty(&json).unwrap())
            .expect("write whitelist JSON");
        eprintln!("Whitelist written to {output_path}");
    }
}
