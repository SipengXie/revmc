//! Dispatch handlers and single-tx replay execution for the ablation bench.
//! Split out of `skelot_ablation_bench.rs`; kept deliberately separate from
//! `timing_handlers.rs` — the metric shapes differ (single total-ns counter
//! here vs per-hash maps there). The parent binary declares `mod bin_common`
//! and the shared structs, referenced via `crate::`.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use revm::database::{CacheDB, EmptyDB};
use revm::handler::{EvmTr, FrameResult, Handler, ItemOrResult};
use revm::primitives::B256;
use revmc_context::{EvmCompilerFn, RawEvmCompilerFn};

use crate::bin_common::{self, snapshot_before_tx, BenchError, BenchEvm, BinLoader, TxBin};
use crate::skelot_common::welch_p;
use revmc::skeleton::build_data_table;
use crate::{CompileMetrics, PerHashArtifact, SkeletonArtifact};

// ── Dispatch / TimingHandlers (adapted from skeleton_vs_perhash_bench.rs) ─

pub fn load_raw_fn(
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

/// Per-hash dispatch: one RawEvmCompilerFn per code hash, each backed by its
/// own .so. Handler calls `f.call_with_interpreter(...)` (no data pointer).
struct TimingPerHashHandler {
    functions: Arc<HashMap<B256, RawEvmCompilerFn>>,
    target_hashes: Arc<HashSet<B256>>,
    /// One entry per target-hash frame executed during this round.
    /// u64 fits any realistic single-frame wall-ns (frames are hundreds of ns
    /// to low ms); saturated via `try_into().unwrap_or(u64::MAX)`.
    frame_ns: Vec<u64>,
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
        let hash = frame.interpreter.bytecode.get_or_calculate_hash();
        let is_target = self.target_hashes.contains(&hash);
        if let Some(&raw_fn) = self.functions.get(&hash) {
            let f = EvmCompilerFn::new(raw_fn);
            let t0 = if is_target { Some(Instant::now()) } else { None };
            let action = unsafe { f.call_with_interpreter(&mut frame.interpreter, ctx) };
            if let Some(t0) = t0 {
                let ns: u64 = t0.elapsed().as_nanos().try_into().unwrap_or(u64::MAX);
                self.frame_ns.push(ns);
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

/// Skeleton dispatch: single shared EvmCompilerFn, per-hash data table.
struct TimingSkeletonHandler {
    dispatch: Arc<HashMap<B256, (EvmCompilerFn, Vec<u8>)>>,
    target_hashes: Arc<HashSet<B256>>,
    /// One entry per target-hash frame executed during this round. See
    /// TimingPerHashHandler::frame_ns for units.
    frame_ns: Vec<u64>,
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
        let hash = frame.interpreter.bytecode.get_or_calculate_hash();
        let is_target = self.target_hashes.contains(&hash);
        if let Some((skel_fn, table)) = self.dispatch.get(&hash) {
            let t0 = if is_target { Some(Instant::now()) } else { None };
            let action = unsafe {
                skel_fn.call_with_interpreter_data(&mut frame.interpreter, ctx, table.as_ptr())
            };
            if let Some(t0) = t0 {
                let ns: u64 = t0.elapsed().as_nanos().try_into().unwrap_or(u64::MAX);
                self.frame_ns.push(ns);
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

// ── Single-tx execution ─────────────────────────────────────────────────────
//
// Each round:
//   1. `single_tx_evm` clones the pre-tx snapshot into a fresh CacheDB and
//      seeds `evm.ctx.tx` with the representative tx.
//   2. A freshly-constructed timing handler runs that single tx.
//   3. Each target-hash frame pushes one wall-ns entry into `h.frame_ns`.
// Per-family `ExecOutput.samples` is the concatenation of every round's
// `frame_ns`, so `len = rounds * frames_per_round_mean` (not `= rounds`).
// The pre-tx snapshot itself is built once per family (outside these loops).

pub struct ExecOutput {
    pub samples: Vec<f64>,
    pub frames_per_round_mean: f64,
    pub frame_samples_n: u64,
}

pub fn exec_per_hash_single(
    snapshot: &CacheDB<EmptyDB>,
    chain_id: Option<u64>,
    tx_bin: &TxBin,
    functions: &Arc<HashMap<B256, RawEvmCompilerFn>>,
    target: &Arc<HashSet<B256>>,
    rounds: usize,
    warmup: usize,
) -> ExecOutput {
    for _ in 0..warmup {
        let mut h = TimingPerHashHandler {
            functions: functions.clone(),
            target_hashes: target.clone(),
            frame_ns: Vec::with_capacity(8),
        };
        let mut evm = bin_common::single_tx_evm(snapshot, chain_id, tx_bin);
        let _ = h.run(&mut evm);
    }
    let mut samples = Vec::with_capacity(rounds * 2);
    let mut total_frames: u64 = 0;
    for _ in 0..rounds {
        let mut h = TimingPerHashHandler {
            functions: functions.clone(),
            target_hashes: target.clone(),
            frame_ns: Vec::with_capacity(8),
        };
        let mut evm = bin_common::single_tx_evm(snapshot, chain_id, tx_bin);
        let _ = h.run(&mut evm);
        total_frames += h.frame_ns.len() as u64;
        samples.extend(h.frame_ns.drain(..).map(|n| n as f64));
    }
    let frames_per_round_mean = total_frames as f64 / rounds as f64;
    ExecOutput { samples, frames_per_round_mean, frame_samples_n: total_frames }
}

pub fn exec_skeleton_single(
    snapshot: &CacheDB<EmptyDB>,
    chain_id: Option<u64>,
    tx_bin: &TxBin,
    dispatch: &Arc<HashMap<B256, (EvmCompilerFn, Vec<u8>)>>,
    target: &Arc<HashSet<B256>>,
    rounds: usize,
    warmup: usize,
) -> ExecOutput {
    for _ in 0..warmup {
        let mut h = TimingSkeletonHandler {
            dispatch: dispatch.clone(),
            target_hashes: target.clone(),
            frame_ns: Vec::with_capacity(8),
        };
        let mut evm = bin_common::single_tx_evm(snapshot, chain_id, tx_bin);
        let _ = h.run(&mut evm);
    }
    let mut samples = Vec::with_capacity(rounds * 2);
    let mut total_frames: u64 = 0;
    for _ in 0..rounds {
        let mut h = TimingSkeletonHandler {
            dispatch: dispatch.clone(),
            target_hashes: target.clone(),
            frame_ns: Vec::with_capacity(8),
        };
        let mut evm = bin_common::single_tx_evm(snapshot, chain_id, tx_bin);
        let _ = h.run(&mut evm);
        total_frames += h.frame_ns.len() as u64;
        samples.extend(h.frame_ns.drain(..).map(|n| n as f64));
    }
    let frames_per_round_mean = total_frames as f64 / rounds as f64;
    ExecOutput { samples, frames_per_round_mean, frame_samples_n: total_frames }
}

pub fn summarize(samples: &[f64]) -> (f64, f64, f64, f64) {
    if samples.is_empty() {
        return (0.0, 0.0, 0.0, 0.0);
    }
    let mut s = samples.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = s.len();
    let median = s[n / 2];
    let p5 = s[((n as f64 * 0.05) as usize).min(n - 1)];
    let p95 = s[((n as f64 * 0.95) as usize).min(n - 1)];
    let mean = s.iter().sum::<f64>() / n as f64;
    let var = s.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n as f64 - 1.0).max(1.0);
    (median, p5, p95, var.sqrt())
}

pub fn flat_mode(
    cm: &CompileMetrics,
    exec: &ExecOutput,
    baseline: Option<&ExecOutput>,
) -> serde_json::Value {
    let samples = exec.samples.as_slice();
    let (median, p5, p95, sd) = summarize(samples);
    let welch = baseline.and_then(|b| {
        if samples.is_empty() || b.samples.is_empty() {
            None
        } else {
            Some(welch_p(samples, &b.samples))
        }
    });
    serde_json::json!({
        "compile_time_ms": cm.compile_time_ms,
        "so_bytes": cm.so_bytes,
        "num_variant": cm.num_variant,
        "num_invariant": cm.num_invariant,
        "num_skip_logic_preserved": cm.num_skip_logic_preserved,
        "exec_median_ns": median,
        "exec_p5_ns": p5,
        "exec_p95_ns": p95,
        "exec_stdev_ns": sd,
        "welch_p_vs_skelot": welch,
        "frames_per_round_mean": exec.frames_per_round_mean,
        "frame_samples_n": exec.frame_samples_n,
    })
}

pub fn geomean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    let log_sum: f64 = xs.iter().map(|v| v.max(1e-12).ln()).sum();
    (log_sum / xs.len() as f64).exp()
}

#[allow(clippy::too_many_arguments)]
pub fn run_family_exec(
    fam_id: &str,
    members: &[(B256, Vec<u8>)],
    rep_candidates: &[(u64, usize)],
    bench_dir: &Path,
    ph_arts: &[PerHashArtifact],
    sk_art: &SkeletonArtifact,
    av_art: &SkeletonArtifact,
    rounds: usize,
    warmup: usize,
) -> (ExecOutput, ExecOutput, ExecOutput) {
    let empty =
        || ExecOutput { samples: Vec::new(), frames_per_round_mean: 0.0, frame_samples_n: 0 };
    if rep_candidates.is_empty() {
        eprintln!("[exec-skip] family {fam_id} has no capped member with a tx in corpus");
        return (empty(), empty(), empty());
    }
    // Walk the candidates: rebuilding pre-tx state replays the block prefix
    // and can fail on a given block (e.g. an OP deposit whose mint the
    // uniform EVM handler does not apply). Any member's tx is an equally
    // valid representative, so fall through to the next one.
    let mut chosen: Option<(BinLoader, usize, _)> = None;
    for &(bn, ti) in rep_candidates {
        let loader = match BinLoader::new(bench_dir, bn) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[rep-try] family {fam_id} block {bn} loader: {e}");
                continue;
            }
        };
        match loader.raw_txs().get(ti) {
            Some(tx) if tx.tx_type == 0x7e => {
                eprintln!("[rep-try] family {fam_id} rep tx[{ti}] @ {bn} is deposit; next");
                continue;
            }
            Some(_) => {}
            None => {
                eprintln!("[rep-try] family {fam_id} tx_index {ti} out of range @ {bn}; next");
                continue;
            }
        }
        match snapshot_before_tx(&loader, ti, false) {
            Ok(s) => {
                chosen = Some((loader, ti, s));
                break;
            }
            Err(e) => {
                eprintln!("[rep-try] family {fam_id} snapshot_before_tx @ {bn}/{ti}: {e}; next");
            }
        }
    }
    let (loader, ti, snapshot) = match chosen {
        Some(c) => c,
        None => {
            eprintln!(
                "[exec-skip] family {fam_id}: all {} candidate rep txs failed",
                rep_candidates.len()
            );
            return (empty(), empty(), empty());
        }
    };
    let chain_id = loader.raw_txs().first().and_then(|tx| tx.chain_id);
    let tx_bin = &loader.raw_txs()[ti];

    // Load .so files
    let mut per_hash_fns: HashMap<B256, RawEvmCompilerFn> = HashMap::new();
    let mut _ph_libs: Vec<libloading::Library> = Vec::new();
    for art in ph_arts {
        match load_raw_fn(&art.lib, &art.symbol) {
            Ok((f, lib)) => {
                per_hash_fns.insert(art.hash, f);
                _ph_libs.push(lib);
            }
            Err(e) => eprintln!("  dlopen per_hash {}: {e}", hex::encode(&art.hash[..4])),
        }
    }

    let (sk_fn, _sk_lib) = match load_raw_fn(&sk_art.lib, &sk_art.symbol) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[exec-skip] family {fam_id} dlopen skelot_auto: {e}");
            return (empty(), empty(), empty());
        }
    };
    let sk_ecf = EvmCompilerFn::new(sk_fn);
    let mut sk_dispatch: HashMap<B256, (EvmCompilerFn, Vec<u8>)> = HashMap::new();
    for (h, bc) in members {
        let table = build_data_table(bc, &sk_art.variance);
        sk_dispatch.insert(*h, (sk_ecf, table.data));
    }

    let (av_fn, _av_lib) = match load_raw_fn(&av_art.lib, &av_art.symbol) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[exec-skip] family {fam_id} dlopen skelot_forced: {e}");
            return (empty(), empty(), empty());
        }
    };
    let av_ecf = EvmCompilerFn::new(av_fn);
    let mut av_dispatch: HashMap<B256, (EvmCompilerFn, Vec<u8>)> = HashMap::new();
    for (h, bc) in members {
        let table = build_data_table(bc, &av_art.variance);
        av_dispatch.insert(*h, (av_ecf, table.data));
    }

    let target: HashSet<B256> = members.iter().map(|(h, _)| *h).collect();
    let target = Arc::new(target);
    let per_hash_fns = Arc::new(per_hash_fns);
    let sk_dispatch = Arc::new(sk_dispatch);
    let av_dispatch = Arc::new(av_dispatch);

    let ph =
        exec_per_hash_single(&snapshot, chain_id, tx_bin, &per_hash_fns, &target, rounds, warmup);
    let sk =
        exec_skeleton_single(&snapshot, chain_id, tx_bin, &sk_dispatch, &target, rounds, warmup);
    let av =
        exec_skeleton_single(&snapshot, chain_id, tx_bin, &av_dispatch, &target, rounds, warmup);

    // Drop Arcs BEFORE `_ph_libs`/`_sk_lib`/`_av_lib` go out of scope so fn
    // pointers stay valid across the exec calls (libs live to the end of
    // this function — do not move those `let _*_lib` bindings later).
    drop(per_hash_fns);
    drop(sk_dispatch);
    drop(av_dispatch);
    drop(target);
    drop(snapshot);
    (ph, sk, av)
}

