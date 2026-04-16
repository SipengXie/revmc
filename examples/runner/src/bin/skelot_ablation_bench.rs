#[path = "../bin_common.rs"]
#[allow(dead_code)]
mod bin_common;

#[path = "../skelot_common.rs"]
#[allow(dead_code)]
mod skelot_common;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;
use op_revm::transaction::OpTransaction;
use op_revm::DefaultOp;
use revm::database::EmptyDB;
use revm::handler::{EvmTr, FrameResult, Handler, ItemOrResult};
use revm::primitives::B256;
use revmc::skeleton::{
    analyze_skeleton_group_with_mode, build_data_table, ClassifyMode, SkeletonVariance,
};
use revmc::OptimizationLevel;
use revmc_context::{EvmCompilerFn, RawEvmCompilerFn};
use serde::{Deserialize, Serialize};

use bin_common::{
    build_op_cfg, build_op_tx, build_tx_index_cached, extract_gas, BenchError, BenchEvm, BinLoader,
    OpCtx,
};
use skelot_common::{
    collect_bytecodes_cached, compile_per_hash, compile_skeleton_group, stratified_family_sample,
    welch_p, FamilySummary,
};

const OPT: OptimizationLevel = OptimizationLevel::Aggressive;

#[derive(Parser, Debug)]
#[command(about = "Stage E — All-Variant SkelOT ablation benchmark")]
struct Args {
    /// Path to Stage A's skeleton_families.json.
    #[arg(long)]
    families: PathBuf,
    /// Bench data root (containing states/ and txs/).
    #[arg(long)]
    bench_dir: PathBuf,
    /// Families per bucket (final: 20; smoke: 1).
    #[arg(long, default_value_t = 20)]
    per_bucket: usize,
    /// Exec rounds (final: 1000; smoke: 50).
    #[arg(long, default_value_t = 1000)]
    rounds: usize,
    /// Exec warmup rounds.
    #[arg(long, default_value_t = 50)]
    warmup: usize,
    /// Welch alpha (reserved, recorded in meta).
    #[arg(long, default_value_t = 0.05)]
    alpha: f64,
    /// Deterministic seed (reserved).
    #[arg(long, default_value_t = 42)]
    seed: u64,
    /// First block (inclusive) whose `states/{bn}.bin` feeds the bytecode corpus.
    #[arg(long, default_value_t = 38004930)]
    start_block: u64,
    /// Number of blocks to scan from `start_block`.
    #[arg(long, default_value_t = 10000)]
    block_count: u64,
    /// Max resolved bytecodes per family for the compile phase and dispatch
    /// target set. per_hash compile cost scales linearly with this; SkelOT /
    /// all_variant compile once per family regardless. Truncation is
    /// deterministic (sorted by hash lex).
    #[arg(long, default_value_t = 128)]
    compile_members_cap: usize,
    /// Output JSON path.
    #[arg(long)]
    out: PathBuf,
    /// Resume from the per-family JSONL sidecar next to `--out`. When `true`
    /// (default), already-completed families skip compile+exec and are
    /// reloaded from the sidecar. Pass `--no-resume` to discard the sidecar
    /// and recompute everything.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    resume: bool,
}

/// Sidecar JSONL path derived from the final output path.
/// `/…/foo.json` → `/…/foo.families.jsonl`.
fn sidecar_path(out: &Path) -> PathBuf {
    let stem = out.file_stem().and_then(|s| s.to_str()).unwrap_or("ablation");
    let parent = out.parent().unwrap_or(Path::new("."));
    parent.join(format!("{stem}.families.jsonl"))
}

/// Read every JSON line from `path` and collect the `skeleton_id` field.
fn load_sidecar_done(path: &Path) -> std::io::Result<(HashSet<String>, Vec<serde_json::Value>)> {
    use std::io::{BufRead, BufReader};
    let mut done = HashSet::new();
    let mut rows = Vec::new();
    let f = std::fs::File::open(path)?;
    for line in BufReader::new(f).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[resume] skipping malformed sidecar line: {e}");
                continue;
            }
        };
        if let Some(id) = v.get("skeleton_id").and_then(|s| s.as_str()) {
            done.insert(id.to_string());
            rows.push(v);
        }
    }
    Ok((done, rows))
}

/// Append one family row as a single JSON line + newline. Creates the file
/// if missing; flushes before returning.
fn append_family_row(path: &Path, row: &serde_json::Value) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    let line = serde_json::to_string(row)?;
    f.write_all(line.as_bytes())?;
    f.write_all(b"\n")?;
    f.flush()?;
    Ok(())
}

#[derive(Debug, Deserialize)]
struct StageAFamily {
    skeleton_hash: String,
    skeleton_len: usize,
    family_size: usize,
}

#[derive(Debug, Deserialize)]
struct StageAFamiliesFile {
    families: Vec<StageAFamily>,
}

fn load_stage_a_families(families_path: &std::path::Path) -> anyhow::Result<Vec<FamilySummary>> {
    let file: StageAFamiliesFile = serde_json::from_reader(std::fs::File::open(families_path)?)?;
    let tsv_path = families_path.with_file_name("skeleton_mapping.tsv");
    let tsv = std::fs::read_to_string(&tsv_path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", tsv_path.display()))?;
    let mut by_skel: HashMap<String, Vec<String>> = HashMap::new();
    for line in tsv.lines() {
        let mut it = line.split('\t');
        if let (Some(s), Some(h)) = (it.next(), it.next()) {
            by_skel.entry(s.to_string()).or_default().push(h.to_string());
        }
    }
    Ok(file
        .families
        .into_iter()
        .map(|f| {
            let member_hashes = by_skel.remove(&f.skeleton_hash).unwrap_or_else(|| {
                eprintln!("[warn] skeleton {} has 0 mapped hashes", f.skeleton_hash);
                Vec::new()
            });
            FamilySummary {
                skeleton_id: f.skeleton_hash,
                skeleton_raw_len: f.skeleton_len,
                member_count: f.family_size,
                member_hashes,
            }
        })
        .collect())
}

#[derive(Debug, Serialize)]
struct CompileMetrics {
    compile_time_ms: f64,
    so_bytes: u64,
    num_variant: u32,
    num_invariant: u32,
    /// PUSHes kept Invariant specifically because their immediate is consumed
    /// by a static JUMP/JUMPI. Meaningful only for ForceAllVariant; 0 for
    /// per_hash and Auto modes.
    num_skip_logic_preserved: u32,
}

fn count_push_immediates(bytecode: &[u8]) -> usize {
    let mut n = 0usize;
    let mut i = 0;
    while i < bytecode.len() {
        let op = bytecode[i];
        i += 1;
        if (0x60..=0x7f).contains(&op) {
            n += 1;
            let len = (op - 0x5f) as usize;
            i = (i + len).min(bytecode.len());
        }
    }
    n
}

fn resolve_family_members(
    all_codes: &HashMap<B256, Vec<u8>>,
    fam: &FamilySummary,
) -> Vec<(String, B256, Vec<u8>)> {
    use std::str::FromStr;
    fam.member_hashes
        .iter()
        .filter_map(|h| {
            let key = B256::from_str(h.trim_start_matches("0x")).ok()?;
            let code = all_codes.get(&key)?;
            Some((h.clone(), key, code.clone()))
        })
        .collect()
}

/// Cap deterministically: members with txs in corpus first (sorted by
/// (earliest block, tx_index)), then fill remainder by hash-sort. Guarantees
/// at least one capped member has a tx if the family has any observations.
fn cap_members(
    resolved: Vec<(String, B256, Vec<u8>)>,
    tx_index: &HashMap<B256, (u64, usize)>,
    cap: usize,
) -> (Vec<(B256, Vec<u8>)>, Option<u64>) {
    let mut with_tx: Vec<(u64, usize, String, B256, Vec<u8>)> = Vec::new();
    let mut without_tx: Vec<(String, B256, Vec<u8>)> = Vec::new();
    for (hstr, key, code) in resolved {
        match tx_index.get(&key) {
            Some(&(bn, ti)) => with_tx.push((bn, ti, hstr, key, code)),
            None => without_tx.push((hstr, key, code)),
        }
    }
    with_tx.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    without_tx.sort_by(|a, b| a.0.cmp(&b.0));
    let rep_block = with_tx.first().map(|(bn, _, _, _, _)| *bn);
    let mut out: Vec<(B256, Vec<u8>)> = with_tx.into_iter().map(|(_, _, _, h, v)| (h, v)).collect();
    if out.len() < cap {
        let need = cap - out.len();
        out.extend(without_tx.into_iter().take(need).map(|(_, h, v)| (h, v)));
    } else {
        out.truncate(cap);
    }
    (out, rep_block)
}

/// Per-hash artifact map keyed by code hash.
struct PerHashArtifact {
    hash: B256,
    lib: PathBuf,
    symbol: String,
}

fn compile_per_hash_mode(
    members: &[(B256, Vec<u8>)],
    tmp_dir: &Path,
) -> anyhow::Result<(CompileMetrics, Vec<PerHashArtifact>)> {
    let t = Instant::now();
    let mut so_bytes = 0u64;
    let mut arts = Vec::with_capacity(members.len());
    for (i, (hash, bc)) in members.iter().enumerate() {
        let symbol = format!("ph_{i}");
        let obj = tmp_dir.join(format!("per_hash_{i}.o"));
        let lib = tmp_dir.join(format!("per_hash_{i}.so"));
        compile_per_hash(bc, &symbol, &obj, &lib, OPT)
            .map_err(|e| anyhow::anyhow!("compile_per_hash: {e}"))?;
        so_bytes += std::fs::metadata(&lib)?.len();
        arts.push(PerHashArtifact { hash: *hash, lib, symbol });
    }
    let total_push = members.first().map(|(_, bc)| count_push_immediates(bc) as u32).unwrap_or(0);
    Ok((
        CompileMetrics {
            compile_time_ms: t.elapsed().as_secs_f64() * 1000.0,
            so_bytes,
            num_variant: 0,
            num_invariant: total_push,
            num_skip_logic_preserved: 0,
        },
        arts,
    ))
}

struct SkeletonArtifact {
    lib: PathBuf,
    symbol: String,
    variance: SkeletonVariance,
}

fn compile_skeleton_mode(
    members: &[(B256, Vec<u8>)],
    mode: ClassifyMode,
    tmp_dir: &Path,
    tag: &str,
) -> anyhow::Result<(CompileMetrics, SkeletonArtifact)> {
    let refs: Vec<&[u8]> = members.iter().map(|(_, v)| v.as_slice()).collect();
    let variance = analyze_skeleton_group_with_mode(&refs, mode);
    let obj = tmp_dir.join(format!("{tag}.o"));
    let lib = tmp_dir.join(format!("{tag}.so"));
    let t = Instant::now();
    compile_skeleton_group(&members[0].1, &variance, tag, &obj, &lib, OPT)
        .map_err(|e| anyhow::anyhow!("compile_skeleton_group: {e}"))?;
    let compile_time_ms = t.elapsed().as_secs_f64() * 1000.0;
    let so_bytes = std::fs::metadata(&lib)?.len();
    let num_total = variance.pushes.len() as u32;
    let num_invariant = num_total - variance.num_variant;
    let num_skip_logic_preserved = match mode {
        ClassifyMode::ForceAllVariant => num_invariant,
        ClassifyMode::Auto => 0,
    };
    Ok((
        CompileMetrics {
            compile_time_ms,
            so_bytes,
            num_variant: variance.num_variant,
            num_invariant,
            num_skip_logic_preserved,
        },
        SkeletonArtifact { lib, symbol: tag.to_string(), variance },
    ))
}

// ── Dispatch / TimingHandlers (adapted from skeleton_vs_perhash_bench.rs) ─

fn load_raw_fn(
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
    total_ns: u128,
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
                self.total_ns += t0.elapsed().as_nanos();
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
    total_ns: u128,
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
                self.total_ns += t0.elapsed().as_nanos();
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

fn replay_block<F: FnMut(&mut BenchEvm) -> (bool, u64)>(
    loader: &BinLoader,
    chain_id: Option<u64>,
    mut run_tx: F,
) {
    let cfg = build_op_cfg(chain_id);
    let dummy_tx = OpTransaction::builder().build_fill();
    let mut evm: BenchEvm = {
        let ctx =
            OpCtx::<EmptyDB>::op().with_cfg(cfg).with_db(loader.build_cache_db()).with_tx(dummy_tx);
        op_revm::OpEvm::new(ctx, ())
    };
    for tx_bin in loader.raw_txs() {
        if tx_bin.tx_type == 0x7e {
            continue;
        }
        evm.0.ctx.tx = build_op_tx(tx_bin);
        let _ = run_tx(&mut evm);
    }
}

fn exec_per_hash(
    loader: &BinLoader,
    chain_id: Option<u64>,
    functions: &Arc<HashMap<B256, RawEvmCompilerFn>>,
    target: &Arc<HashSet<B256>>,
    rounds: usize,
    warmup: usize,
) -> Vec<f64> {
    for _ in 0..warmup {
        let mut h = TimingPerHashHandler {
            functions: functions.clone(),
            target_hashes: target.clone(),
            total_ns: 0,
        };
        replay_block(loader, chain_id, |evm| match h.run(evm) {
            Ok(rr) => extract_gas(&rr),
            Err(_) => (false, 0),
        });
    }
    let mut samples = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let mut h = TimingPerHashHandler {
            functions: functions.clone(),
            target_hashes: target.clone(),
            total_ns: 0,
        };
        replay_block(loader, chain_id, |evm| match h.run(evm) {
            Ok(rr) => extract_gas(&rr),
            Err(_) => (false, 0),
        });
        samples.push(h.total_ns as f64);
    }
    samples
}

fn exec_skeleton(
    loader: &BinLoader,
    chain_id: Option<u64>,
    dispatch: &Arc<HashMap<B256, (EvmCompilerFn, Vec<u8>)>>,
    target: &Arc<HashSet<B256>>,
    rounds: usize,
    warmup: usize,
) -> Vec<f64> {
    for _ in 0..warmup {
        let mut h = TimingSkeletonHandler {
            dispatch: dispatch.clone(),
            target_hashes: target.clone(),
            total_ns: 0,
        };
        replay_block(loader, chain_id, |evm| match h.run(evm) {
            Ok(rr) => extract_gas(&rr),
            Err(_) => (false, 0),
        });
    }
    let mut samples = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let mut h = TimingSkeletonHandler {
            dispatch: dispatch.clone(),
            target_hashes: target.clone(),
            total_ns: 0,
        };
        replay_block(loader, chain_id, |evm| match h.run(evm) {
            Ok(rr) => extract_gas(&rr),
            Err(_) => (false, 0),
        });
        samples.push(h.total_ns as f64);
    }
    samples
}

fn summarize(samples: &[f64]) -> (f64, f64, f64, f64) {
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

fn flat_mode(cm: &CompileMetrics, samples: &[f64], baseline: Option<&[f64]>) -> serde_json::Value {
    let (median, p5, p95, sd) = summarize(samples);
    let welch = baseline.and_then(|b| {
        if samples.is_empty() || b.is_empty() {
            None
        } else {
            Some(welch_p(samples, b))
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
    })
}

fn geomean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    let log_sum: f64 = xs.iter().map(|v| v.max(1e-12).ln()).sum();
    (log_sum / xs.len() as f64).exp()
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let families = load_stage_a_families(&args.families)?;
    let sampled = stratified_family_sample(&families, args.per_bucket, args.seed);
    eprintln!("Sampled {} families from {} total", sampled.len(), families.len());

    // Resumable persistence: each completed family is appended as one JSON
    // line to `{out-stem}.families.jsonl` next to --out. On restart we read
    // the sidecar, skip already-done skeleton_ids, and at end reload all
    // rows from it to build bucket_aggregates and the final JSON.
    let sidecar = sidecar_path(&args.out);
    if !args.resume {
        let _ = std::fs::remove_file(&sidecar);
        eprintln!("[resume] --no-resume: cleared sidecar {}", sidecar.display());
    }
    let (done_ids, prior_rows) = load_sidecar_done(&sidecar).unwrap_or_default();
    if !done_ids.is_empty() {
        eprintln!(
            "[resume] loaded {} prior family rows from {}",
            prior_rows.len(),
            sidecar.display()
        );
    }
    let all_done = sampled.iter().all(|f| done_ids.contains(&f.skeleton_id));

    // Skip the corpus scan + tx-index build entirely when the sidecar already
    // covers every sampled family. Otherwise, load the cached indices so
    // subsequent runs reuse the scan result instead of paying ~8 min per
    // restart. Each entry is `(capped_members, earliest_rep_block)`.
    let family_plans: Vec<(Vec<(B256, Vec<u8>)>, Option<u64>)> = if all_done {
        eprintln!(
            "[resume] all {} sampled families already in sidecar; skipping corpus scan",
            sampled.len()
        );
        vec![(Vec::new(), None); sampled.len()]
    } else {
        let all_codes =
            collect_bytecodes_cached(&args.bench_dir, args.start_block, args.block_count);
        eprintln!(
            "Collected {} compilable bytecodes from states/[{}..{})",
            all_codes.len(),
            args.start_block,
            args.start_block + args.block_count
        );

        // Two-phase cap: resolve ALL family members, build tx-index over them,
        // then cap preferring members with txs in corpus. Guarantees the
        // capped set has a rep whenever the family has any observed txs.
        let resolved_per_family: Vec<Vec<(String, B256, Vec<u8>)>> =
            sampled.iter().map(|fam| resolve_family_members(&all_codes, fam)).collect();
        let tx_index = build_tx_index_cached(&args.bench_dir, args.start_block, args.block_count);

        resolved_per_family
            .into_iter()
            .map(|r| cap_members(r, &tx_index, args.compile_members_cap))
            .collect()
    };

    let tmp_root = tempfile::tempdir()?;

    for (idx, (fam, (members, rep_block_from_cap))) in
        sampled.iter().zip(family_plans.iter()).enumerate()
    {
        if done_ids.contains(&fam.skeleton_id) {
            eprintln!(
                "[{}/{}] [resume] skipping already-done family {}",
                idx + 1,
                sampled.len(),
                fam.skeleton_id
            );
            continue;
        }
        if members.is_empty() {
            eprintln!("[skip] family {}: no bytecodes resolved from corpus", fam.skeleton_id);
            continue;
        }
        let fam_tmp = tmp_root.path().join(&fam.skeleton_id);
        std::fs::create_dir_all(&fam_tmp)?;

        // ── Compile (3 modes) ─────────────────────────────────────────────
        let (per_hash, ph_arts) = compile_per_hash_mode(members, &fam_tmp)?;
        let (skelot, sk_art) =
            compile_skeleton_mode(members, ClassifyMode::Auto, &fam_tmp, "skelot_auto")?;
        let (all_variant, av_art) = compile_skeleton_mode(
            members,
            ClassifyMode::ForceAllVariant,
            &fam_tmp,
            "skelot_forced",
        )?;

        let bucket_name = match skelot_common::bucket_for_raw_len(fam.skeleton_raw_len) {
            0 => "tiny",
            1 => "small",
            2 => "medium",
            3 => "large",
            _ => "huge",
        };

        // Rep block is computed during cap_members so we don't need tx_index
        // in this scope (the all-done fast path doesn't build it).
        let rep_block: Option<u64> = *rep_block_from_cap;

        let (ph_samples, sk_samples, av_samples): (Vec<f64>, Vec<f64>, Vec<f64>) = if let Some(bn) =
            rep_block
        {
            match BinLoader::new(&args.bench_dir, bn) {
                Ok(loader) => {
                    let chain_id = loader.raw_txs().first().and_then(|tx| tx.chain_id);

                    // Load .so files
                    let mut per_hash_fns: HashMap<B256, RawEvmCompilerFn> = HashMap::new();
                    let mut _ph_libs: Vec<libloading::Library> = Vec::new();
                    for art in &ph_arts {
                        match load_raw_fn(&art.lib, &art.symbol) {
                            Ok((f, lib)) => {
                                per_hash_fns.insert(art.hash, f);
                                _ph_libs.push(lib);
                            }
                            Err(e) => {
                                eprintln!("  dlopen per_hash {}: {e}", hex::encode(&art.hash[..4]))
                            }
                        }
                    }

                    let (sk_fn, _sk_lib) = load_raw_fn(&sk_art.lib, &sk_art.symbol)
                        .map_err(|e| anyhow::anyhow!("dlopen skelot_auto: {e}"))?;
                    let sk_ecf = EvmCompilerFn::new(sk_fn);
                    let mut sk_dispatch: HashMap<B256, (EvmCompilerFn, Vec<u8>)> = HashMap::new();
                    for (h, bc) in members {
                        let table = build_data_table(bc, &sk_art.variance);
                        sk_dispatch.insert(*h, (sk_ecf, table.data));
                    }

                    let (av_fn, _av_lib) = load_raw_fn(&av_art.lib, &av_art.symbol)
                        .map_err(|e| anyhow::anyhow!("dlopen skelot_forced: {e}"))?;
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

                    let ph = exec_per_hash(
                        &loader,
                        chain_id,
                        &per_hash_fns,
                        &target,
                        args.rounds,
                        args.warmup,
                    );
                    let sk = exec_skeleton(
                        &loader,
                        chain_id,
                        &sk_dispatch,
                        &target,
                        args.rounds,
                        args.warmup,
                    );
                    let av = exec_skeleton(
                        &loader,
                        chain_id,
                        &av_dispatch,
                        &target,
                        args.rounds,
                        args.warmup,
                    );

                    // Drop dispatch/Arc BEFORE _libs go out of scope to keep
                    // function pointers valid during the replay (guaranteed
                    // by Arc ordering — libs live to end of scope).
                    drop(per_hash_fns);
                    drop(sk_dispatch);
                    drop(av_dispatch);
                    drop(target);
                    (ph, sk, av)
                }
                Err(e) => {
                    eprintln!("[exec-skip] family {} block {bn} loader: {e}", fam.skeleton_id);
                    (Vec::new(), Vec::new(), Vec::new())
                }
            }
        } else {
            eprintln!(
                "[exec-skip] family {} has no capped member with a tx in corpus",
                fam.skeleton_id
            );
            (Vec::new(), Vec::new(), Vec::new())
        };

        // ── Flatten ───────────────────────────────────────────────────────
        let ph_row = flat_mode(&per_hash, &ph_samples, Some(&sk_samples));
        let sk_row = flat_mode(&skelot, &sk_samples, None);
        let av_row = flat_mode(&all_variant, &av_samples, Some(&sk_samples));

        let row = serde_json::json!({
            "skeleton_id": fam.skeleton_id,
            "bucket": bucket_name,
            "member_count": fam.member_count,
            "members_compiled": members.len(),
            "per_hash": ph_row,
            "skelot": sk_row,
            "all_variant": av_row,
        });
        append_family_row(&sidecar, &row)?;
        eprintln!(
            "[{}/{}] {} done (rep_block={:?}, samples=[ph={}, sk={}, av={}])",
            idx + 1,
            sampled.len(),
            fam.skeleton_id,
            rep_block,
            ph_samples.len(),
            sk_samples.len(),
            av_samples.len()
        );
    }

    // Reload the authoritative family rows from the sidecar. On a plain run
    // this is a superset of what we just appended; on resume it's the union
    // of prior + new rows. Source of truth for bucket_aggregates and the
    // final JSON.
    let (_final_ids, fam_rows) = load_sidecar_done(&sidecar).unwrap_or_default();
    eprintln!("Assembling final output from {} sidecar rows", fam_rows.len());

    // ── Bucket aggregates ─────────────────────────────────────────────────
    let mut by_bucket: std::collections::BTreeMap<String, Vec<&serde_json::Value>> =
        Default::default();
    for row in &fam_rows {
        let b = row["bucket"].as_str().unwrap_or("?").to_string();
        by_bucket.entry(b).or_default().push(row);
    }
    let mut aggregates = Vec::new();
    for (bucket, rows) in by_bucket {
        let ct: Vec<f64> = rows
            .iter()
            .map(|r| {
                r["all_variant"]["compile_time_ms"].as_f64().unwrap_or(0.0)
                    / r["skelot"]["compile_time_ms"].as_f64().unwrap_or(1e-9).max(1e-9)
            })
            .collect();
        let so: Vec<f64> = rows
            .iter()
            .map(|r| {
                r["all_variant"]["so_bytes"].as_f64().unwrap_or(0.0)
                    / r["skelot"]["so_bytes"].as_f64().unwrap_or(1.0).max(1.0)
            })
            .collect();
        let ex: Vec<f64> = rows
            .iter()
            .filter_map(|r| {
                let av = r["all_variant"]["exec_median_ns"].as_f64()?;
                let sk = r["skelot"]["exec_median_ns"].as_f64()?;
                if sk > 0.0 {
                    Some(av / sk)
                } else {
                    None
                }
            })
            .collect();
        let mut ps: Vec<f64> = rows
            .iter()
            .filter_map(|r| r["all_variant"]["welch_p_vs_skelot"].as_f64())
            .filter(|v| !v.is_nan())
            .collect();
        ps.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p_med = if ps.is_empty() { 1.0 } else { ps[ps.len() / 2] };

        aggregates.push(serde_json::json!({
            "bucket": bucket,
            "n": rows.len(),
            "compile_time_geomean_ratio_allvariant_over_skelot": geomean(&ct),
            "so_bytes_geomean_ratio_allvariant_over_skelot": geomean(&so),
            "exec_median_geomean_ratio_allvariant_over_skelot":
                if ex.is_empty() { 0.0 } else { geomean(&ex) },
            "welch_p_median": p_med,
        }));
    }

    let out = serde_json::json!({
        "meta": {
            "git_sha": std::env::var("GIT_SHA").unwrap_or_else(|_| "unknown".into()),
            "ts_utc": chrono::Utc::now().to_rfc3339(),
            "corpus_source": args.families.display().to_string(),
            "sample_size": sampled.len(),
            "per_bucket": args.per_bucket,
            "rounds": args.rounds,
            "warmup": args.warmup,
            "alpha": args.alpha,
            "compile_members_cap": args.compile_members_cap,
        },
        "families": fam_rows,
        "bucket_aggregates": aggregates,
    });
    std::fs::write(&args.out, serde_json::to_string_pretty(&out)?)?;
    eprintln!("Wrote output to {}", args.out.display());
    Ok(())
}
