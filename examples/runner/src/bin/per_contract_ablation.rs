//! Per-contract E[speedup] ablation: read native_ranking.bin, find blocks
//! containing each top-N contract, run leave-one-out ablation across multiple
//! blocks, report per-contract expected speedup with statistical confidence.
//!
//! Uses skeleton-aware cache loading: skeleton groups load from skeleton .so
//! + data tables, singletons from per-hash .so. Zero compilation needed.
//!
//! Usage:
//!   cargo run -p revmc-examples-runner --bin per_contract_ablation --release -- \
//!     --ranking /path/to/native_ranking.bin \
//!     --top-n 20 --blocks-per 5 --rounds 10 --warmup 3

#[allow(dead_code)]
#[path = "../bin_common.rs"]
mod bin_common;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use op_revm::OpHaltReason;
use revm::{
    bytecode::Bytecode,
    handler::{EvmTr, FrameResult, Handler, ItemOrResult},
    primitives::B256,
};
use revmc::skeleton::{analyze_skeleton_group, build_data_table, SkeletonVariance};
use revmc_builtins as _;
use revmc_context::{EvmCompilerFn, RawEvmCompilerFn};

use bin_common::{
    build_op_tx, make_evm, should_lookup_jit, BenchError, BenchEvm, BinLoader, CacheSnapshot,
    NativeHandler, ETH_SPEC,
};

// ── CLI ─────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "per_contract_ablation", about = "Per-contract E[speedup] ablation across multiple blocks")]
struct Args {
    /// Path to native_ranking.bin (bincode Vec<(PathKeyCompat, u64)>)
    #[arg(long)]
    ranking: String,
    #[arg(long, default_value = "/home/ubuntu/sipeng/bench_data")]
    dir: String,
    #[arg(long, default_value = "./jit_cache")]
    cache_dir: String,
    /// Number of top contracts to test
    #[arg(long, default_value_t = 20)]
    top_n: usize,
    /// Blocks per contract
    #[arg(long, default_value_t = 5)]
    blocks_per: usize,
    /// Measurement rounds per block
    #[arg(long, default_value_t = 10)]
    rounds: usize,
    /// Warmup rounds per block
    #[arg(long, default_value_t = 3)]
    warmup: usize,
    /// Significance level for Welch's t-test
    #[arg(long, default_value_t = 0.05)]
    alpha: f64,
    /// Optional JSON output path
    #[arg(long)]
    output: Option<String>,
}

// ── Ranking Loading ─────────────────────────────────────────────────────────

/// Compatible deserialization for PathKey from revm-compact-plan.
/// PathKey.code_hash is U256, which bincode serializes identically to B256
/// (both use serialize_bytes with 32 BE bytes).
#[derive(serde::Deserialize, Debug)]
#[cfg_attr(test, derive(serde::Serialize))]
struct PathKeyCompat {
    code_hash: B256,
    #[allow(dead_code)]
    path_hash: u64,
}

/// Parse ranking bytes, trying 3-tuple (PathKey, u32, u64) first, then legacy 2-tuple.
/// Returns sorted descending by aggregated native_ns, truncated to top_n.
fn load_ranking_bytes(data: &[u8], top_n: usize) -> Option<Vec<(B256, u64)>> {
    // Try 3-tuple format: (PathKeyCompat, u32, u64) — current producer
    let entries: Vec<(B256, u64)> =
        if let Ok(v) = bincode::deserialize::<Vec<(PathKeyCompat, u32, u64)>>(data) {
            v.into_iter().map(|(pk, _, ns)| (pk.code_hash, ns)).collect()
        } else if let Ok(v) = bincode::deserialize::<Vec<(PathKeyCompat, u64)>>(data) {
            v.into_iter().map(|(pk, ns)| (pk.code_hash, ns)).collect()
        } else {
            return None;
        };

    let mut by_hash: HashMap<B256, u64> = HashMap::new();
    for (hash, ns) in &entries {
        *by_hash.entry(*hash).or_insert(0) += ns;
    }

    let mut sorted: Vec<(B256, u64)> = by_hash.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));
    sorted.truncate(top_n);
    Some(sorted)
}

/// Load ranking from file, aggregate by code_hash (sum native_ns), return sorted top-N.
fn load_ranking(path: &str, top_n: usize) -> Vec<(B256, u64)> {
    let data = std::fs::read(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let sorted = load_ranking_bytes(&data, top_n)
        .unwrap_or_else(|| panic!("deserialize {path}: unknown format"));

    eprintln!("  Loaded ranking, {} unique contracts", sorted.len());
    eprintln!("  Top-{} contracts by native time:", sorted.len());
    for (i, (hash, ns)) in sorted.iter().enumerate() {
        eprintln!(
            "    #{:>2}: 0x{}..  {:.1}ms",
            i + 1,
            &hex::encode(hash)[..16],
            *ns as f64 / 1e6
        );
    }

    sorted
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn shuffle<T>(slice: &mut [T], mut rng: u64) -> u64 {
    for i in (1..slice.len()).rev() {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
        let j = (rng >> 33) as usize % (i + 1);
        slice.swap(i, j);
    }
    rng
}

fn time_seed() -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        .hash(&mut hasher);
    hasher.finish()
}

fn parallel_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().min(16))
        .unwrap_or(4)
}

// ── Skeleton Registry ───────────────────────────────────────────────────────

/// Registry format saved by skeleton_precompile.
#[derive(serde::Deserialize)]
struct SkeletonRegistry {
    groups: Vec<RegistryGroup>,
}

#[derive(serde::Deserialize)]
struct RegistryGroup {
    skeleton_hash: u64,
    member_hashes: Vec<[u8; 32]>,
    num_variant: u32,
    #[allow(dead_code)]
    num_members: u32,
}

/// Pre-computed skeleton info with reconstructed variance.
struct SkeletonGroupInfo {
    skeleton_hash: u64,
    #[allow(dead_code)]
    members: Vec<B256>,
    variance: SkeletonVariance,
}

/// All skeleton info needed for cache loading.
struct SkeletonInfo {
    /// code_hash → skeleton group index
    member_to_group: HashMap<B256, usize>,
    groups: Vec<SkeletonGroupInfo>,
}

fn load_skeleton_info(
    cache_dir: &Path,
    bench_dir: &Path,
    block_files: &[u64],
) -> Option<(SkeletonInfo, HashMap<B256, Vec<u8>>)> {
    let registry_path = cache_dir.join("skeleton_registry.bin");
    let data = match std::fs::read(&registry_path) {
        Ok(d) => d,
        Err(_) => {
            eprintln!("  No skeleton_registry.bin found, using per-hash only");
            return None;
        }
    };
    let registry: SkeletonRegistry = match bincode::deserialize(&data) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("  Failed to parse registry: {e}");
            return None;
        }
    };

    eprintln!("  Registry: {} skeleton groups", registry.groups.len());

    // Build member set from registry
    let mut member_to_skel: HashMap<B256, (usize, u64)> = HashMap::new();
    for (gi, group) in registry.groups.iter().enumerate() {
        for raw_hash in &group.member_hashes {
            let hash = B256::from(*raw_hash);
            member_to_skel.insert(hash, (gi, group.skeleton_hash));
        }
    }
    let member_set: HashSet<B256> = member_to_skel.keys().copied().collect();
    let total_members = member_set.len();

    // Collect bytecodes for skeleton members by scanning blocks
    eprintln!(
        "  Collecting bytecodes for {} skeleton members across {} blocks...",
        total_members,
        block_files.len()
    );
    let n_threads = parallel_threads();
    let chunk_size = block_files.len().div_ceil(n_threads);
    let chunks: Vec<&[u64]> = block_files.chunks(chunk_size).collect();

    let thread_results: Vec<HashMap<B256, Vec<u8>>> = std::thread::scope(|s| {
        let handles: Vec<_> = chunks
            .into_iter()
            .map(|chunk| {
                let member_set = &member_set;
                s.spawn(move || {
                    let mut local: HashMap<B256, Vec<u8>> = HashMap::new();
                    for &block_num in chunk {
                        let path = bench_dir.join(format!("states/{block_num}.bin"));
                        let data = match std::fs::read(&path) {
                            Ok(d) => d,
                            Err(_) => continue,
                        };
                        let snapshot: CacheSnapshot = match bincode::deserialize(&data) {
                            Ok(s) => s,
                            Err(_) => continue,
                        };
                        for (hash, bytecode) in &snapshot.codes {
                            if member_set.contains(hash) && !local.contains_key(hash) {
                                if !bytecode.is_empty() {
                                    local.insert(
                                        *hash,
                                        bytecode.original_byte_slice().to_vec(),
                                    );
                                }
                            }
                        }
                    }
                    local
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut member_bytecodes: HashMap<B256, Vec<u8>> = HashMap::new();
    for local in thread_results {
        for (hash, bytes) in local {
            member_bytecodes.entry(hash).or_insert(bytes);
        }
    }
    eprintln!(
        "  Collected {}/{} member bytecodes",
        member_bytecodes.len(),
        total_members
    );

    // Re-analyze variance per group
    let mut groups: Vec<SkeletonGroupInfo> = Vec::with_capacity(registry.groups.len());
    let mut member_to_group: HashMap<B256, usize> = HashMap::new();

    for reg_group in &registry.groups {
        let members: Vec<B256> = reg_group
            .member_hashes
            .iter()
            .map(|h| B256::from(*h))
            .collect();

        // Collect bytecodes for this group's members
        let bytecodes: Vec<&[u8]> = members
            .iter()
            .filter_map(|h| member_bytecodes.get(h).map(|b| b.as_slice()))
            .collect();

        if bytecodes.len() < 2 {
            // Can't analyze with < 2 members, skip this group (per-hash fallback)
            continue;
        }

        let variance = analyze_skeleton_group(&bytecodes);

        // Sanity check: num_variant should match registry
        if variance.num_variant != reg_group.num_variant {
            eprintln!(
                "  WARN: skel {:016x} variance mismatch: registry={} analyzed={} (have {}/{} members)",
                reg_group.skeleton_hash,
                reg_group.num_variant,
                variance.num_variant,
                bytecodes.len(),
                members.len()
            );
            // Use the analyzed variance — it matches the bytecodes we have
        }

        let gi = groups.len();
        for hash in &members {
            if member_bytecodes.contains_key(hash) {
                member_to_group.insert(*hash, gi);
            }
        }

        groups.push(SkeletonGroupInfo {
            skeleton_hash: reg_group.skeleton_hash,
            members,
            variance,
        });
    }

    eprintln!(
        "  {} skeleton groups ready, {} members mapped",
        groups.len(),
        member_to_group.len()
    );

    Some((
        SkeletonInfo {
            member_to_group,
            groups,
        },
        member_bytecodes,
    ))
}

// ── Skeleton-Aware JIT Loading ──────────────────────────────────────────────

/// Unified JIT entry: function pointer + optional data table for skeleton dispatch.
type JitFn = (RawEvmCompilerFn, Option<Arc<Vec<u8>>>);

fn opt_tag() -> &'static str {
    "o3" // OptimizationLevel::Aggressive
}

fn per_hash_cache_path(cache_dir: &Path, hash: &B256) -> PathBuf {
    let hash_hex = hex::encode(hash);
    let spec_tag = format!("{ETH_SPEC:?}").to_lowercase();
    cache_dir.join(format!("{hash_hex}__{spec_tag}__{}.so", opt_tag()))
}

fn skeleton_cache_path(cache_dir: &Path, skeleton_hash: u64) -> PathBuf {
    let spec_tag = format!("{ETH_SPEC:?}").to_lowercase();
    cache_dir.join(format!(
        "skel_{skeleton_hash:016x}__{spec_tag}__{}.so",
        opt_tag()
    ))
}

struct JitBundle {
    functions: Arc<HashMap<B256, JitFn>>,
    _libraries: Vec<libloading::Library>,
}

/// Load JIT functions for a block's contracts from skeleton-aware cache.
fn load_jit_for_block(
    codes: &HashMap<B256, Bytecode>,
    cache_dir: &Path,
    skeleton_info: Option<&SkeletonInfo>,
    member_bytecodes: &HashMap<B256, Vec<u8>>,
) -> JitBundle {
    let mut functions: HashMap<B256, JitFn> = HashMap::new();
    let mut libraries: Vec<libloading::Library> = Vec::new();

    // Cache loaded skeleton .so to avoid re-loading for each member
    let mut skeleton_fn_cache: HashMap<u64, Option<RawEvmCompilerFn>> = HashMap::new();

    let mut per_hash_loaded = 0usize;
    let mut skeleton_loaded = 0usize;
    let mut missed = 0usize;

    for (hash, bytecode) in codes {
        if bytecode.is_empty() {
            continue;
        }

        // Try skeleton dispatch first
        if let Some(skel_info) = skeleton_info {
            if let Some(&gi) = skel_info.member_to_group.get(hash) {
                let group = &skel_info.groups[gi];
                let skel_hash = group.skeleton_hash;

                // Load skeleton .so (cached per skeleton_hash)
                let raw_fn = skeleton_fn_cache
                    .entry(skel_hash)
                    .or_insert_with(|| {
                        let lib_path = skeleton_cache_path(cache_dir, skel_hash);
                        let symbol = format!("skel_{skel_hash:016x}");
                        match load_symbol(&lib_path, &symbol) {
                            Ok((f, lib)) => {
                                libraries.push(lib);
                                Some(f)
                            }
                            Err(_) => None,
                        }
                    })
                    .as_ref()
                    .copied();

                if let Some(raw_fn) = raw_fn {
                    // Build data table for this specific member
                    let bytes = member_bytecodes
                        .get(hash)
                        .map(|b| b.as_slice())
                        .unwrap_or(bytecode.original_byte_slice());
                    let table = build_data_table(bytes, &group.variance);
                    functions.insert(*hash, (raw_fn, Some(Arc::new(table.data))));
                    skeleton_loaded += 1;
                    continue;
                }
                // Fall through to per-hash if skeleton .so not found
            }
        }

        // Per-hash cache
        let lib_path = per_hash_cache_path(cache_dir, hash);
        let symbol = format!("c_{}", hex::encode(hash));
        match load_symbol(&lib_path, &symbol) {
            Ok((f, lib)) => {
                libraries.push(lib);
                functions.insert(*hash, (f, None));
                per_hash_loaded += 1;
            }
            Err(_) => missed += 1,
        }
    }

    eprintln!(
        "    JIT loaded: {} skeleton + {} per-hash ({} missed)",
        skeleton_loaded, per_hash_loaded, missed
    );

    JitBundle {
        functions: Arc::new(functions),
        _libraries: libraries,
    }
}

fn load_symbol(
    lib_path: &Path,
    symbol_name: &str,
) -> Result<(RawEvmCompilerFn, libloading::Library), String> {
    let library = unsafe { libloading::Library::new(lib_path) }
        .map_err(|e| format!("dlopen {}: {e}", lib_path.display()))?;
    let symbol = unsafe { library.get::<RawEvmCompilerFn>(symbol_name.as_bytes()) }
        .map_err(|e| format!("dlsym {symbol_name}: {e}"))?;
    let function = *symbol;
    drop(symbol);
    Ok((function, library))
}

// ── Block Index Building ────────────────────────────────────────────────────

/// Scan bench_data/states/ to find blocks containing target code_hashes.
fn build_block_index(
    bench_dir: &Path,
    targets: &[B256],
    blocks_per: usize,
) -> (HashMap<B256, Vec<u64>>, Vec<u64>) {
    let states_dir = bench_dir.join("states");
    let mut block_files: Vec<u64> = std::fs::read_dir(&states_dir)
        .unwrap_or_else(|e| panic!("read {:?}: {e}", states_dir))
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name();
            let name = name.to_str()?;
            let num = name.strip_suffix(".bin")?.parse::<u64>().ok()?;
            bench_dir
                .join(format!("txs/{num}.bin"))
                .exists()
                .then_some(num)
        })
        .collect();

    let mut rng = time_seed();
    rng = shuffle(&mut block_files, rng);

    eprintln!(
        "  Scanning {} block files for {} targets (need {} blocks each)...",
        block_files.len(),
        targets.len(),
        blocks_per
    );

    let target_set: HashSet<B256> = targets.iter().copied().collect();
    let n_threads = parallel_threads();
    let chunk_size = block_files.len().div_ceil(n_threads);
    let chunks: Vec<Vec<u64>> = block_files
        .chunks(chunk_size)
        .map(|c| c.to_vec())
        .collect();

    let scanned = std::sync::atomic::AtomicU64::new(0);

    let thread_results: Vec<Vec<(B256, u64)>> = std::thread::scope(|s| {
        let handles: Vec<_> = chunks
            .into_iter()
            .map(|chunk| {
                let target_set = &target_set;
                let scanned = &scanned;
                let bench_dir = bench_dir;
                s.spawn(move || {
                    let mut results: Vec<(B256, u64)> = Vec::new();
                    for block_num in chunk {
                        let states_path = bench_dir.join(format!("states/{block_num}.bin"));
                        let data = match std::fs::read(&states_path) {
                            Ok(d) => d,
                            Err(_) => continue,
                        };
                        let snapshot: CacheSnapshot = match bincode::deserialize(&data) {
                            Ok(s) => s,
                            Err(_) => continue,
                        };
                        for code_hash in snapshot.codes.keys() {
                            if target_set.contains(code_hash) {
                                results.push((*code_hash, block_num));
                            }
                        }
                        let n =
                            scanned.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        if n % 500 == 0 {
                            eprintln!("    scanned {n} blocks...");
                        }
                    }
                    results
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut index: HashMap<B256, Vec<u64>> = HashMap::new();
    for results in thread_results {
        for (hash, block_num) in results {
            let blocks = index.entry(hash).or_default();
            if !blocks.contains(&block_num) {
                blocks.push(block_num);
            }
        }
    }

    for blocks in index.values_mut() {
        rng = shuffle(blocks, rng);
        blocks.truncate(blocks_per);
    }

    for target in targets {
        let count = index.get(target).map_or(0, |v| v.len());
        if count == 0 {
            eprintln!(
                "  WARNING: 0x{}.. found in 0 blocks",
                &hex::encode(target)[..16]
            );
        }
    }

    (index, block_files)
}

// ── Unified JIT Handler ─────────────────────────────────────────────────────

/// Run the current frame using unified JitFn (per-hash or skeleton+data_table).
fn run_unified_jit_or_native(
    evm: &mut BenchEvm,
    functions: &HashMap<B256, JitFn>,
) -> Result<
    ItemOrResult<revm::interpreter::interpreter_action::FrameInit, FrameResult>,
    BenchError,
> {
    let (ctx, frame_stack) = (&mut evm.0.ctx, &mut evm.0.frame_stack);
    let frame = frame_stack.get();

    if !should_lookup_jit(
        frame.data.is_create(),
        frame.interpreter.input.bytecode_address,
        frame.interpreter.bytecode.is_empty(),
    ) {
        drop((ctx, frame_stack));
        return Ok(evm.frame_run()?);
    }

    let bytecode_hash = frame.interpreter.bytecode.get_or_calculate_hash();
    if let Some((raw_fn, data_table)) = functions.get(&bytecode_hash) {
        let f = EvmCompilerFn::new(*raw_fn);
        let action = if let Some(data) = data_table {
            unsafe { f.call_with_interpreter_data(&mut frame.interpreter, ctx, data.as_ptr()) }
        } else {
            unsafe { f.call_with_interpreter(&mut frame.interpreter, ctx) }
        };
        let result = frame
            .process_next_action::<_, BenchError>(ctx, action)
            .inspect(|i| {
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

struct UnifiedJitHandler {
    functions: Arc<HashMap<B256, JitFn>>,
}

impl Handler for UnifiedJitHandler {
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
        loop {
            let call_or_result = run_unified_jit_or_native(evm, &self.functions)?;
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

// ── Timing JIT Handler ──────────────────────────────────────────────────────

/// Handler that times each frame segment individually, accumulating per-hash durations.
struct TimingJitHandler {
    functions: Arc<HashMap<B256, JitFn>>,
    per_hash_time: HashMap<B256, Duration>,
}

impl TimingJitHandler {
    fn new(functions: Arc<HashMap<B256, JitFn>>) -> Self {
        Self {
            functions,
            per_hash_time: HashMap::new(),
        }
    }

    fn into_times(self) -> HashMap<B256, Duration> {
        self.per_hash_time
    }

    /// Read the current frame's bytecode hash (B256::ZERO if not JIT-eligible).
    fn current_frame_hash(evm: &mut BenchEvm) -> B256 {
        let frame = evm.0.frame_stack.get();
        if should_lookup_jit(
            frame.data.is_create(),
            frame.interpreter.input.bytecode_address,
            frame.interpreter.bytecode.is_empty(),
        ) {
            frame.interpreter.bytecode.get_or_calculate_hash()
        } else {
            B256::ZERO
        }
    }
}

impl Handler for TimingJitHandler {
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
        loop {
            let hash = Self::current_frame_hash(evm);

            let t0 = Instant::now();
            let call_or_result = run_unified_jit_or_native(evm, &self.functions)?;
            let elapsed = t0.elapsed();

            if hash != B256::ZERO {
                *self.per_hash_time.entry(hash).or_insert(Duration::ZERO) += elapsed;
            }

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

// ── Full-Block Replay ───────────────────────────────────────────────────────

fn run_full_block(
    loader: &BinLoader,
    functions: Option<&Arc<HashMap<B256, JitFn>>>,
) -> Vec<Duration> {
    let chain_id = loader.raw_txs().first().and_then(|tx| tx.chain_id);
    let mut evm = make_evm(loader, chain_id);

    let mut per_tx = Vec::with_capacity(loader.tx_count());
    for tx_bin in loader.raw_txs() {
        if tx_bin.tx_type == 0x7e {
            per_tx.push(Duration::ZERO);
            continue;
        }
        evm.0.ctx.tx = build_op_tx(tx_bin);
        let t0 = Instant::now();
        if let Some(fns) = functions {
            let mut h = UnifiedJitHandler {
                functions: fns.clone(),
            };
            let _ = h.run(&mut evm);
        } else {
            let mut h = NativeHandler;
            let _ = h.run(&mut evm);
        }
        per_tx.push(t0.elapsed());
    }
    per_tx
}

/// Run all txs in a block with per-frame timing, returning per-hash total durations.
fn run_full_block_timed(
    loader: &BinLoader,
    functions: &Arc<HashMap<B256, JitFn>>,
) -> HashMap<B256, Duration> {
    let chain_id = loader.raw_txs().first().and_then(|tx| tx.chain_id);
    let mut evm = make_evm(loader, chain_id);
    let mut block_times: HashMap<B256, Duration> = HashMap::new();

    for tx_bin in loader.raw_txs() {
        if tx_bin.tx_type == 0x7e {
            continue;
        }
        evm.0.ctx.tx = build_op_tx(tx_bin);
        let mut handler = TimingJitHandler::new(functions.clone());
        let _ = handler.run(&mut evm);
        for (hash, dur) in handler.into_times() {
            *block_times.entry(hash).or_insert(Duration::ZERO) += dur;
        }
    }
    block_times
}

/// Warmup with untimed replay, then collect per-hash frame-level timings.
fn collect_timed_samples(
    loader: &BinLoader,
    functions: &Arc<HashMap<B256, JitFn>>,
    warmup: usize,
    rounds: usize,
) -> Vec<HashMap<B256, Duration>> {
    for _ in 0..warmup {
        run_full_block(loader, Some(functions));
    }
    (0..rounds)
        .map(|_| run_full_block_timed(loader, functions))
        .collect()
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
    let cache_dir = Path::new(&args.cache_dir);

    eprintln!("=== Per-Contract E[Speedup] Ablation ===");
    eprintln!(
        "  top_n={}, blocks_per={}, rounds={}, warmup={}, alpha={}",
        args.top_n, args.blocks_per, args.rounds, args.warmup, args.alpha
    );

    // ── Step 1: Load ranking ────────────────────────────────────────────────
    eprintln!("\n=== Step 1: Load ranking ===");
    let ranking = load_ranking(&args.ranking, args.top_n);
    let target_hashes: Vec<B256> = ranking.iter().map(|(h, _)| *h).collect();
    let target_native_ms: HashMap<B256, f64> = ranking
        .iter()
        .map(|(h, ns)| (*h, *ns as f64 / 1e6))
        .collect();

    // ── Step 2: Build block index ───────────────────────────────────────────
    eprintln!("\n=== Step 2: Build block index ===");
    let (block_index, all_block_files) =
        build_block_index(bench_dir, &target_hashes, args.blocks_per);

    let active_targets: Vec<B256> = target_hashes
        .iter()
        .filter(|h| block_index.get(*h).map_or(false, |v| !v.is_empty()))
        .copied()
        .collect();
    eprintln!(
        "  {} / {} targets have blocks",
        active_targets.len(),
        target_hashes.len()
    );

    if active_targets.is_empty() {
        eprintln!("ERROR: no targets found in any block");
        return;
    }

    // ── Step 2b: Load skeleton info ─────────────────────────────────────────
    eprintln!("\n=== Step 2b: Load skeleton info ===");
    let (skeleton_info, member_bytecodes) =
        match load_skeleton_info(cache_dir, bench_dir, &all_block_files) {
            Some((info, bytecodes)) => (Some(info), bytecodes),
            None => (None, HashMap::new()),
        };

    // ── Step 3: Ablation ────────────────────────────────────────────────────
    let mut block_to_targets: HashMap<u64, Vec<usize>> = HashMap::new();
    for (ti, hash) in active_targets.iter().enumerate() {
        if let Some(blocks) = block_index.get(hash) {
            for &block_num in blocks {
                block_to_targets.entry(block_num).or_default().push(ti);
            }
        }
    }

    let unique_blocks: Vec<u64> = {
        let mut v: Vec<u64> = block_to_targets.keys().copied().collect();
        v.sort();
        v
    };
    eprintln!(
        "\n=== Step 3: Ablation across {} unique blocks ===",
        unique_blocks.len()
    );

    let mut per_target_deltas: Vec<Vec<f64>> = vec![Vec::new(); active_targets.len()];
    let mut per_target_ratios: Vec<Vec<f64>> = vec![Vec::new(); active_targets.len()];

    for (bi, &block_num) in unique_blocks.iter().enumerate() {
        let targets_in_block = &block_to_targets[&block_num];
        eprintln!(
            "\n  Block {} ({}/{}) — {} targets",
            block_num,
            bi + 1,
            unique_blocks.len(),
            targets_in_block.len()
        );

        let loader = match BinLoader::new(bench_dir, block_num) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("    WARN: skip block {block_num}: {e}");
                continue;
            }
        };

        // Load JIT from skeleton-aware cache (zero compilation)
        let all_codes: HashMap<B256, _> = loader
            .code_values()
            .iter()
            .map(|(h, b)| (*h, b.clone()))
            .collect();
        let bundle = load_jit_for_block(
            &all_codes,
            cache_dir,
            skeleton_info.as_ref(),
            &member_bytecodes,
        );
        let all_functions = bundle.functions;
        let _libs = bundle._libraries;

        // Baseline (frame-level timing)
        let baseline_samples =
            collect_timed_samples(&loader, &all_functions, args.warmup, args.rounds);

        // Per-target ablation
        for &ti in targets_in_block {
            let target_hash = active_targets[ti];

            // Skip if target was never executed in this block
            let has_time = baseline_samples
                .iter()
                .any(|s| s.get(&target_hash).map_or(false, |d| *d > Duration::ZERO));
            if !has_time {
                eprintln!(
                    "    target 0x{}.. not executed in block {block_num}, skip",
                    &hex::encode(target_hash)[..12]
                );
                continue;
            }

            // Ablated map: remove target (Arc<Vec<u8>> clone is cheap)
            let ablated_fns: Arc<HashMap<B256, JitFn>> = Arc::new(
                all_functions
                    .iter()
                    .filter(|(&h, _)| h != target_hash)
                    .map(|(&h, v)| (h, v.clone()))
                    .collect(),
            );

            let ablated_samples =
                collect_timed_samples(&loader, &ablated_fns, args.warmup, args.rounds);

            // Frame-level delta: mean(ablated[target]) - mean(baseline[target])
            let mean_baseline: f64 = baseline_samples
                .iter()
                .map(|s| s.get(&target_hash).map_or(0.0, |d| d.as_secs_f64() * 1e6))
                .sum::<f64>()
                / args.rounds as f64;
            let mean_ablated: f64 = ablated_samples
                .iter()
                .map(|s| s.get(&target_hash).map_or(0.0, |d| d.as_secs_f64() * 1e6))
                .sum::<f64>()
                / args.rounds as f64;

            let block_delta = mean_ablated - mean_baseline;
            let block_ratio = if mean_baseline > 0.0 {
                mean_ablated / mean_baseline
            } else {
                1.0
            };

            per_target_deltas[ti].push(block_delta);
            per_target_ratios[ti].push(block_ratio);

            eprintln!(
                "    0x{}..  baseline={:.1}us  ablated={:.1}us  delta={:+.1}us  ratio={:.3}",
                &hex::encode(target_hash)[..12],
                mean_baseline,
                mean_ablated,
                block_delta,
                block_ratio
            );
        }
    }

    // ── Step 4: Cross-block aggregation ─────────────────────────────────────
    eprintln!("\n=== Step 4: Cross-block aggregation ===");

    struct ContractResult {
        hash: B256,
        native_ms: f64,
        n_blocks: usize,
        mean_delta_us: f64,
        mean_ratio: f64,
        p_value: f64,
        verdict: &'static str,
    }

    let mut results: Vec<ContractResult> = Vec::new();

    for (ti, &hash) in active_targets.iter().enumerate() {
        let deltas = &per_target_deltas[ti];
        let ratios = &per_target_ratios[ti];
        if deltas.is_empty() {
            continue;
        }

        let mean_delta = deltas.iter().sum::<f64>() / deltas.len() as f64;
        let mean_ratio = ratios.iter().sum::<f64>() / ratios.len() as f64;

        let (p_val, v) = if deltas.len() >= 2 {
            let zeros = vec![0.0f64; deltas.len()];
            if let Some(wr) = welch_t_test(deltas, &zeros) {
                (wr.p_value, verdict(wr.p_value, wr.mean_diff, args.alpha))
            } else {
                (1.0, "NEUTRAL")
            }
        } else {
            (1.0, "NEUTRAL")
        };

        results.push(ContractResult {
            hash,
            native_ms: target_native_ms.get(&hash).copied().unwrap_or(0.0),
            n_blocks: deltas.len(),
            mean_delta_us: mean_delta,
            mean_ratio,
            p_value: p_val,
            verdict: v,
        });
    }

    results.sort_by(|a, b| b.mean_delta_us.partial_cmp(&a.mean_delta_us).unwrap());

    // ── Output ──────────────────────────────────────────────────────────────
    println!(
        "\n=== Per-Contract E[Speedup] Ablation (top-{}, {}-block avg) ===\n",
        args.top_n, args.blocks_per
    );
    println!(
        "{:>16}  {:>10}  {:>6}  {:>12}  {:>8}  {:>8}  {}",
        "Contract", "Native(ms)", "Blocks", "Delta(us)", "Ratio", "p_value", "Verdict"
    );

    let mut n_good = 0usize;
    let mut n_bad = 0usize;
    let mut n_neutral = 0usize;
    let mut whitelist: Vec<String> = Vec::new();
    let mut blacklist: Vec<String> = Vec::new();

    for r in &results {
        let short = &hex::encode(r.hash)[..12];
        println!(
            "{short:>16}  {:>10.1}  {:>6}  {:>+12.1}  {:>8.3}  {:>8.4}  {}",
            r.native_ms, r.n_blocks, r.mean_delta_us, r.mean_ratio, r.p_value, r.verdict
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

    println!("\nJIT-GOOD: {n_good} | JIT-BAD: {n_bad} | NEUTRAL: {n_neutral}");

    if let Some(output_path) = &args.output {
        let json = serde_json::json!({
            "top_n": args.top_n,
            "blocks_per": args.blocks_per,
            "rounds": args.rounds,
            "alpha": args.alpha,
            "results": results.iter().map(|r| {
                serde_json::json!({
                    "code_hash": format!("0x{}", hex::encode(r.hash)),
                    "native_ms": r.native_ms,
                    "blocks": r.n_blocks,
                    "mean_delta_us": r.mean_delta_us,
                    "mean_ratio": r.mean_ratio,
                    "p_value": r.p_value,
                    "verdict": r.verdict,
                })
            }).collect::<Vec<_>>(),
            "whitelist": whitelist,
            "blacklist": blacklist,
            "summary": {
                "good": n_good,
                "bad": n_bad,
                "neutral": n_neutral
            }
        });
        std::fs::write(output_path, serde_json::to_string_pretty(&json).unwrap())
            .expect("write JSON output");
        eprintln!("Results written to {output_path}");
    }
}


#[cfg(test)]
mod tests {
    use revm::primitives::B256;

    use super::{load_ranking_bytes, PathKeyCompat};

    fn mk_hash(byte: u8) -> B256 {
        B256::from([byte; 32])
    }

    #[test]
    fn load_ranking_bytes_accepts_trace_side_three_tuple_schema() {
        let entries: Vec<(PathKeyCompat, u32, u64)> = vec![
            (PathKeyCompat { code_hash: mk_hash(0x11), path_hash: 1 }, 0x12345678, 100),
            (PathKeyCompat { code_hash: mk_hash(0x11), path_hash: 2 }, 0x12345678, 250),
            (PathKeyCompat { code_hash: mk_hash(0x22), path_hash: 3 }, 0x87654321, 90),
        ];
        let bytes = bincode::serialize(&entries).expect("serialize");

        let ranking = load_ranking_bytes(&bytes, 10).expect("should parse three-tuple schema");

        assert_eq!(ranking, vec![(mk_hash(0x11), 350), (mk_hash(0x22), 90)]);
    }

    #[test]
    fn load_ranking_bytes_accepts_legacy_two_tuple_schema() {
        let entries: Vec<(PathKeyCompat, u64)> = vec![
            (PathKeyCompat { code_hash: mk_hash(0x33), path_hash: 1 }, 40),
            (PathKeyCompat { code_hash: mk_hash(0x33), path_hash: 2 }, 60),
            (PathKeyCompat { code_hash: mk_hash(0x44), path_hash: 3 }, 55),
        ];
        let bytes = bincode::serialize(&entries).expect("serialize");

        let ranking = load_ranking_bytes(&bytes, 10).expect("should parse legacy schema");

        assert_eq!(ranking, vec![(mk_hash(0x33), 100), (mk_hash(0x44), 55)]);
    }
}
