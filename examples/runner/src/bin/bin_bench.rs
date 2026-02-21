//! Multi-block JIT benchmark: revmc JIT vs native revm interpreter.
//!
//! Loads prestate from CacheSnapshot (.bin) and transactions from BlockBin (.bin),
//! then compiles all unique contracts via LLVM JIT and benchmarks execution.
//!
//! Usage:
//!   cargo run -p revmc-examples-runner --bin bin_bench --release -- --start 38004930 --count 10
//!   cargo run -p revmc-examples-runner --bin bin_bench --release -- --start 38004930 --end 38004940
//!   cargo run -p revmc-examples-runner --bin bin_bench --release -- --start 38004930 --count 5 --cache-dir /tmp/jit_cache

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use op_revm::{DefaultOp, OpEvm, OpHaltReason, OpSpecId, OpTransactionError};
use op_revm::transaction::OpTransaction;
use revm::{
    bytecode::Bytecode,
    context::{CfgEnv, TxEnv},
    context_interface::result::{EVMError, ExecutionResult},
    database::{CacheDB, EmptyDB},
    handler::{EvmTr, FrameResult, Handler, ItemOrResult},
    primitives::{
        hardfork::SpecId, Address, Bytes, HashMap as RevmHashMap,
        TxKind, B256, U256,
    },
    state::AccountInfo,
};
use revmc::{EvmCompiler, EvmLlvmBackend, OptimizationLevel};
use revmc_builtins as _;
use revmc_context::{EvmCompilerFn, RawEvmCompilerFn};
use serde::Deserialize;

type OpCtx<DB> = op_revm::OpContext<DB>;

// ── Spec Constants ──────────────────────────────────────────────────────────

const OP_SPEC: OpSpecId = OpSpecId::ISTHMUS;
const ETH_SPEC: SpecId = OP_SPEC.into_eth_spec();

// ── Binary Data Types ───────────────────────────────────────────────────────

#[derive(Clone, Deserialize)]
struct AccountSnapshot {
    info: Option<AccountInfo>,
    storage: RevmHashMap<U256, U256>,
}

#[derive(Clone, Deserialize)]
struct CacheSnapshot {
    block_number: u64,
    #[allow(dead_code)]
    has_state_clear: bool,
    accounts: RevmHashMap<Address, AccountSnapshot>,
    codes: RevmHashMap<B256, Bytecode>,
}

#[derive(Deserialize)]
struct BlockBin {
    #[allow(dead_code)]
    block_number: u64,
    txs: Vec<TxBin>,
}

#[derive(Deserialize)]
struct TxBin {
    tx_type: u8,
    caller: [u8; 20],
    gas_limit: u64,
    gas_price: u128,
    to: Option<[u8; 20]>,
    value: [u8; 32],
    data: Vec<u8>,
    nonce: u64,
    chain_id: Option<u64>,
    #[allow(dead_code)]
    access_list: Vec<AccessListItemBin>,
    #[allow(dead_code)]
    gas_priority_fee: Option<u128>,
    #[allow(dead_code)]
    max_fee_per_blob_gas: u128,
    source_hash: Option<[u8; 32]>,
    mint: Option<[u8; 32]>,
    is_system_tx: bool,
}

#[derive(Deserialize)]
struct AccessListItemBin {
    #[allow(dead_code)]
    address: [u8; 20],
    #[allow(dead_code)]
    storage_keys: Vec<[u8; 32]>,
}

impl TxBin {
    fn to_tx_env(&self) -> TxEnv {
        let caller = Address::from_slice(&self.caller);
        let value = U256::from_be_bytes(self.value);
        let kind = match self.to {
            Some(addr) => TxKind::Call(Address::from_slice(&addr)),
            None => TxKind::Create,
        };
        TxEnv {
            caller,
            gas_limit: self.gas_limit,
            gas_price: self.gas_price,
            kind,
            value,
            data: Bytes::from(self.data.clone()),
            nonce: self.nonce,
            chain_id: self.chain_id,
            ..Default::default()
        }
    }
}

// ── BinLoader ───────────────────────────────────────────────────────────────

struct BinLoader {
    snapshot: CacheSnapshot,
    block_bin: BlockBin,
}

impl BinLoader {
    fn new(bench_dir: &Path, block_number: u64) -> Result<Self, String> {
        let states_path = bench_dir.join(format!("states/{block_number}.bin"));
        let txs_path = bench_dir.join(format!("txs/{block_number}.bin"));

        let snapshot_data =
            std::fs::read(&states_path).map_err(|e| format!("read {:?}: {e}", states_path))?;
        let snapshot: CacheSnapshot = bincode::deserialize(&snapshot_data)
            .map_err(|e| format!("deserialize states/{block_number}.bin: {e}"))?;

        let txs_data =
            std::fs::read(&txs_path).map_err(|e| format!("read {:?}: {e}", txs_path))?;
        let block_bin: BlockBin = bincode::deserialize(&txs_data)
            .map_err(|e| format!("deserialize txs/{block_number}.bin: {e}"))?;

        Ok(Self { snapshot, block_bin })
    }

    fn block_number(&self) -> u64 {
        self.snapshot.block_number
    }

    fn tx_count(&self) -> usize {
        self.block_bin.txs.len()
    }

    fn account_count(&self) -> usize {
        self.snapshot.accounts.len()
    }

    fn code_count(&self) -> usize {
        self.snapshot.codes.len()
    }

    fn raw_txs(&self) -> &[TxBin] {
        &self.block_bin.txs
    }

    fn code_values(&self) -> &RevmHashMap<B256, Bytecode> {
        &self.snapshot.codes
    }

    fn build_cache_db(&self) -> CacheDB<EmptyDB> {
        let mut db = CacheDB::new(EmptyDB::new());

        for (addr, acc) in &self.snapshot.accounts {
            if let Some(info) = &acc.info {
                let mut acc_info = info.clone();
                if acc_info.code.is_none() {
                    acc_info.code = self.snapshot.codes.get(&info.code_hash).cloned();
                }
                db.insert_account_info(*addr, acc_info);

                if !acc.storage.is_empty() {
                    let storage: RevmHashMap<_, _> = acc.storage.clone();
                    db.replace_account_storage(*addr, storage).ok();
                }
            }
        }

        for (code_hash, bytecode) in &self.snapshot.codes {
            db.cache.contracts.insert(*code_hash, bytecode.clone());
        }

        db
    }
}

// ── OP Builders ─────────────────────────────────────────────────────────────

fn build_op_cfg(chain_id: Option<u64>) -> CfgEnv<OpSpecId> {
    let mut cfg = CfgEnv::new_with_spec(OP_SPEC);
    cfg.tx_chain_id_check = false;
    if let Some(id) = chain_id {
        cfg.chain_id = id;
    }
    cfg
}

fn build_op_tx(tx: &TxBin) -> OpTransaction<TxEnv> {
    let tx_env = tx.to_tx_env();
    let mut op_tx = OpTransaction::new(tx_env);
    op_tx.enveloped_tx = Some(Bytes::new());
    if tx.tx_type == 0x7e {
        if let Some(src) = tx.source_hash {
            op_tx.deposit.source_hash = B256::from(src);
        }
        if let Some(mint) = tx.mint {
            op_tx.deposit.mint = Some(U256::from_be_bytes(mint).to::<u128>());
        }
        op_tx.deposit.is_system_transaction = tx.is_system_tx;
    }
    op_tx
}

fn extract_gas(result: &ExecutionResult<OpHaltReason>) -> (bool, u64) {
    match result {
        ExecutionResult::Success { gas_used, .. } => (true, *gas_used),
        ExecutionResult::Revert { gas_used, .. } => (false, *gas_used),
        ExecutionResult::Halt { gas_used, .. } => (false, *gas_used),
    }
}

// ── Handlers ────────────────────────────────────────────────────────────────

type BenchEvm = OpEvm<op_revm::OpContext<CacheDB<EmptyDB>>, ()>;
type BenchError = EVMError<core::convert::Infallible, OpTransactionError>;

struct NativeHandler;

impl Handler for NativeHandler {
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
            let call_or_result = evm.frame_run()?;
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

struct JitHandler {
    functions: Arc<HashMap<B256, RawEvmCompilerFn>>,
}

impl Handler for JitHandler {
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
            let call_or_result = {
                let frame = evm.0.frame_stack.get();
                let bytecode_hash = frame.interpreter.bytecode.get_or_calculate_hash();
                if let Some(&raw_fn) = self.functions.get(&bytecode_hash) {
                    let f = EvmCompilerFn::new(raw_fn);
                    let action =
                        unsafe { f.call_with_interpreter(&mut frame.interpreter, &mut evm.0.ctx) };
                    let frame = evm.0.frame_stack.get();
                    frame
                        .process_next_action::<_, BenchError>(&mut evm.0.ctx, action)
                        .inspect(|i| {
                            if i.is_result() {
                                evm.0.frame_stack.get().set_finished(true);
                            }
                        })?
                } else {
                    evm.frame_run()?
                }
            };
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

// ── Compilation ─────────────────────────────────────────────────────────────

struct CompiledContracts {
    functions: Arc<HashMap<B256, RawEvmCompilerFn>>,
    _libraries: Vec<libloading::Library>,
}

/// Collect unique bytecodes across multiple BinLoaders, deduplicated by code hash.
fn collect_unique_bytecodes(loaders: &[BinLoader]) -> HashMap<B256, Bytecode> {
    let mut all_codes: HashMap<B256, Bytecode> = HashMap::new();
    for loader in loaders {
        for (hash, bytecode) in loader.code_values() {
            all_codes.entry(*hash).or_insert_with(|| bytecode.clone());
        }
    }
    all_codes
}

fn compile_all_contracts(code_values: &HashMap<B256, Bytecode>) -> CompiledContracts {
    let mut contracts: Vec<(B256, &Bytecode)> = code_values
        .iter()
        .filter(|(_, bc)| !bc.is_empty())
        .map(|(h, bc)| (*h, bc))
        .collect();
    contracts.sort_by_key(|(_, bc)| std::cmp::Reverse(bc.original_byte_slice().len()));

    let empty_count = code_values.len() - contracts.len();
    let n_threads = std::thread::available_parallelism()
        .map(|n| n.get().min(16))
        .unwrap_or(4);
    eprintln!(
        "  Using {n_threads} threads for {}/{} contracts ({empty_count} empty)",
        contracts.len(),
        code_values.len()
    );

    let mut assignments: Vec<Vec<(B256, &Bytecode)>> = vec![vec![]; n_threads];
    for (i, contract) in contracts.into_iter().enumerate() {
        assignments[i % n_threads].push(contract);
    }

    let thread_results: Vec<(HashMap<B256, RawEvmCompilerFn>, usize)> =
        std::thread::scope(|s| {
            let handles: Vec<_> = assignments
                .into_iter()
                .enumerate()
                .map(|(tid, chunk)| {
                    s.spawn(move || {
                        let context = Box::leak(Box::new(
                            revmc::llvm::inkwell::context::Context::create(),
                        ));
                        let backend =
                            EvmLlvmBackend::new(context, false, OptimizationLevel::Aggressive)
                                .expect("LLVM backend");
                        let compiler: &'static mut EvmCompiler<EvmLlvmBackend<'static>> =
                            Box::leak(Box::new(EvmCompiler::new(backend)));

                        let mut pending = Vec::new();
                        let mut skipped = 0usize;
                        for (hash, bytecode) in &chunk {
                            let name = format!("c_{}", hex::encode(&hash.as_slice()[..8]));
                            match compiler.translate(
                                &name,
                                bytecode.original_byte_slice(),
                                ETH_SPEC,
                            ) {
                                Ok(func_id) => pending.push((*hash, func_id)),
                                Err(e) => {
                                    eprintln!("  [T{tid}] WARN translate: {name}: {e}");
                                    skipped += 1;
                                }
                            }
                        }

                        let mut functions = HashMap::with_capacity(pending.len());
                        for (hash, func_id) in pending {
                            match unsafe { compiler.jit_function(func_id) } {
                                Ok(fn_ptr) => {
                                    functions.insert(hash, fn_ptr.into_inner());
                                }
                                Err(e) => {
                                    let short = hex::encode(&hash.as_slice()[..4]);
                                    eprintln!("  [T{tid}] WARN JIT: {short}: {e}");
                                    skipped += 1;
                                }
                            }
                        }
                        eprintln!(
                            "  [T{tid}] done: {}/{} compiled",
                            functions.len(),
                            chunk.len()
                        );
                        (functions, skipped)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

    let mut all_functions = HashMap::new();
    let mut total_skipped = empty_count;
    for (fns, skipped) in thread_results {
        all_functions.extend(fns);
        total_skipped += skipped;
    }

    eprintln!(
        "  Total compiled: {}/{} ({total_skipped} skipped)",
        all_functions.len(),
        code_values.len()
    );

    CompiledContracts {
        functions: Arc::new(all_functions),
        _libraries: Vec::new(),
    }
}

// ── AOT Cache ───────────────────────────────────────────────────────────────

struct CacheArtifacts {
    key: String,
    symbol: String,
    object: PathBuf,
    library: PathBuf,
}

fn cache_artifacts(cache_dir: &Path, hash: &B256, opt: OptimizationLevel) -> CacheArtifacts {
    let hash_hex = hex::encode(hash);
    let spec_tag = format!("{ETH_SPEC:?}").to_lowercase();
    let opt_tag = match opt {
        OptimizationLevel::None => "o0",
        OptimizationLevel::Less => "o1",
        OptimizationLevel::Default => "o2",
        OptimizationLevel::Aggressive => "o3",
    };
    let key = format!("{hash_hex}__{spec_tag}__{opt_tag}");
    let stem = cache_dir.join(&key);
    CacheArtifacts {
        key,
        symbol: format!("c_{hash_hex}"),
        object: stem.with_extension("o"),
        library: stem.with_extension(std::env::consts::DLL_EXTENSION),
    }
}

fn load_cached_symbol(
    artifacts: &CacheArtifacts,
) -> Result<(RawEvmCompilerFn, libloading::Library), String> {
    let library = unsafe { libloading::Library::new(&artifacts.library) }
        .map_err(|e| format!("dlopen {}: {e}", artifacts.library.display()))?;
    let symbol = unsafe { library.get::<RawEvmCompilerFn>(artifacts.symbol.as_bytes()) }
        .map_err(|e| format!("dlsym {}: {e}", artifacts.symbol))?;
    let function = *symbol;
    drop(symbol);
    Ok((function, library))
}

fn compile_contract_to_cache(
    bytecode: &Bytecode,
    opt: OptimizationLevel,
    artifacts: &CacheArtifacts,
) -> Result<(), String> {
    if let Some(parent) = artifacts.object.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let context = Box::leak(Box::new(revmc::llvm::inkwell::context::Context::create()));
    let backend = EvmLlvmBackend::new(context, true, opt)
        .map_err(|e| format!("AOT backend: {e}"))?;
    let mut compiler = EvmCompiler::new(backend);
    compiler
        .translate(&artifacts.symbol, bytecode.original_byte_slice(), ETH_SPEC)
        .map_err(|e| format!("translate {}: {e}", artifacts.key))?;
    compiler
        .write_object_to_file(&artifacts.object)
        .map_err(|e| format!("write {}: {e}", artifacts.object.display()))?;
    revmc::Linker::new()
        .link(&artifacts.library, [&artifacts.object])
        .map_err(|e| format!("link {}: {e}", artifacts.library.display()))?;
    Ok(())
}

fn compile_jit_fallback(
    hash: B256,
    bytecode: &Bytecode,
    opt: OptimizationLevel,
) -> Option<RawEvmCompilerFn> {
    let symbol = format!("c_{}", hex::encode(hash));
    let context = Box::leak(Box::new(revmc::llvm::inkwell::context::Context::create()));
    let backend = EvmLlvmBackend::new(context, false, opt).ok()?;
    let compiler: &'static mut EvmCompiler<EvmLlvmBackend<'static>> =
        Box::leak(Box::new(EvmCompiler::new(backend)));
    let func_id = compiler
        .translate(&symbol, bytecode.original_byte_slice(), ETH_SPEC)
        .ok()?;
    unsafe { compiler.jit_function(func_id).ok().map(|f| f.into_inner()) }
}

fn compile_all_contracts_with_cache(
    code_values: &HashMap<B256, Bytecode>,
    opt: OptimizationLevel,
    cache_dir: &Path,
) -> CompiledContracts {
    std::fs::create_dir_all(cache_dir).ok();

    let mut contracts: Vec<(B256, &Bytecode)> = code_values
        .iter()
        .filter(|(_, bc)| !bc.is_empty())
        .map(|(h, bc)| (*h, bc))
        .collect();
    contracts.sort_by_key(|(_, bc)| std::cmp::Reverse(bc.original_byte_slice().len()));

    let mut cached = Vec::new();
    let mut to_compile: Vec<(B256, &Bytecode, CacheArtifacts)> = Vec::new();
    for (hash, bytecode) in &contracts {
        let artifacts = cache_artifacts(cache_dir, hash, opt);
        if artifacts.object.exists() && artifacts.library.exists() {
            cached.push((*hash, artifacts));
        } else {
            to_compile.push((*hash, *bytecode, artifacts));
        }
    }

    let mut functions = HashMap::new();
    let mut libraries = Vec::new();
    let mut hits = 0usize;
    for (hash, artifacts) in cached {
        match load_cached_symbol(&artifacts) {
            Ok((f, lib)) => {
                functions.insert(hash, f);
                libraries.push(lib);
                hits += 1;
            }
            Err(e) => {
                eprintln!("  Cache stale {}..: {e}", &hex::encode(hash)[..16]);
                let bytecode = code_values.get(&hash).unwrap();
                to_compile.push((hash, bytecode, artifacts));
            }
        }
    }

    let n_threads = std::thread::available_parallelism()
        .map(|n| n.get().min(16))
        .unwrap_or(4);
    eprintln!(
        "  Cache: hits={hits} to_compile={} threads={n_threads}",
        to_compile.len()
    );

    if to_compile.is_empty() {
        return CompiledContracts {
            functions: Arc::new(functions),
            _libraries: libraries,
        };
    }

    let mut assignments: Vec<Vec<(B256, &Bytecode, CacheArtifacts)>> =
        (0..n_threads).map(|_| Vec::new()).collect();
    for (i, item) in to_compile.into_iter().enumerate() {
        assignments[i % n_threads].push(item);
    }

    let thread_results: Vec<Vec<(B256, RawEvmCompilerFn, Option<libloading::Library>)>> =
        std::thread::scope(|s| {
            let handles: Vec<_> = assignments
                .into_iter()
                .enumerate()
                .map(|(tid, chunk)| {
                    s.spawn(move || {
                        let mut results = Vec::with_capacity(chunk.len());
                        for (hash, bytecode, artifacts) in chunk {
                            if let Err(e) = compile_contract_to_cache(bytecode, opt, &artifacts) {
                                eprintln!(
                                    "  [T{tid}] AOT fail {}..: {e}",
                                    &hex::encode(hash)[..16]
                                );
                                if let Some(f) = compile_jit_fallback(hash, bytecode, opt) {
                                    results.push((hash, f, None));
                                }
                                continue;
                            }
                            match load_cached_symbol(&artifacts) {
                                Ok((f, lib)) => results.push((hash, f, Some(lib))),
                                Err(e) => {
                                    eprintln!(
                                        "  [T{tid}] load fail {}..: {e}",
                                        &hex::encode(hash)[..16]
                                    );
                                    if let Some(f) = compile_jit_fallback(hash, bytecode, opt) {
                                        results.push((hash, f, None));
                                    }
                                }
                            }
                        }
                        results
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

    for thread_result in thread_results {
        for (hash, f, lib) in thread_result {
            functions.insert(hash, f);
            if let Some(lib) = lib {
                libraries.push(lib);
            }
        }
    }

    eprintln!("  Total: {} compiled (hits={hits})", functions.len());

    CompiledContracts {
        functions: Arc::new(functions),
        _libraries: libraries,
    }
}

// ── Block Execution ─────────────────────────────────────────────────────────

struct BlockResult {
    native_results: Vec<(bool, u64)>,
    jit_results: Vec<(bool, u64)>,
    native_dur: Duration,
    jit_dur: Duration,
}

/// Run a full block: for each tx, execute native then JIT on independent EVMs.
/// Both EVMs accumulate state across txs (nonce, balance, storage updates).
fn run_block(
    loader: &BinLoader,
    functions: &Arc<HashMap<B256, RawEvmCompilerFn>>,
) -> Result<BlockResult, String> {
    let chain_id = loader.raw_txs().first().and_then(|tx| tx.chain_id);
    let cfg = build_op_cfg(chain_id);

    // Two independent CacheDBs so native and JIT accumulate state independently.
    let native_db = loader.build_cache_db();
    let jit_db = loader.build_cache_db();

    // NOTE: with_cfg BEFORE with_db so the journal gets the correct spec id.
    let dummy_tx = OpTransaction::builder().build_fill();
    let native_ctx = OpCtx::<EmptyDB>::op()
        .with_cfg(cfg.clone())
        .with_db(native_db)
        .with_tx(dummy_tx.clone());
    let jit_ctx = OpCtx::<EmptyDB>::op()
        .with_cfg(cfg)
        .with_db(jit_db)
        .with_tx(dummy_tx);

    let mut native_evm: BenchEvm = OpEvm::new(native_ctx, ());
    let mut jit_evm: BenchEvm = OpEvm::new(jit_ctx, ());

    let mut native_results = Vec::with_capacity(loader.tx_count());
    let mut jit_results = Vec::with_capacity(loader.tx_count());
    let mut native_dur = Duration::ZERO;
    let mut jit_dur = Duration::ZERO;

    for (i, tx_bin) in loader.raw_txs().iter().enumerate() {
        if tx_bin.tx_type == 0x7e {
            native_results.push((true, 0));
            jit_results.push((true, 0));
            continue;
        }
        let op_tx = build_op_tx(tx_bin);

        // Native: set tx in-place and run (state accumulates in native_evm's journal)
        native_evm.0.ctx.tx = op_tx.clone();
        let mut handler = NativeHandler;
        let t0 = Instant::now();
        match handler.run(&mut native_evm) {
            Ok(result) => {
                native_dur += t0.elapsed();
                native_results.push(extract_gas(&result));
            }
            Err(e) => {
                native_dur += t0.elapsed();
                eprintln!("  native tx[{i}] error: {e:?}");
                native_results.push((false, 0));
            }
        }

        // JIT: set tx in-place and run (state accumulates in jit_evm's journal)
        jit_evm.0.ctx.tx = op_tx;
        let mut handler = JitHandler {
            functions: functions.clone(),
        };
        let t0 = Instant::now();
        match handler.run(&mut jit_evm) {
            Ok(result) => {
                jit_dur += t0.elapsed();
                jit_results.push(extract_gas(&result));
            }
            Err(e) => {
                jit_dur += t0.elapsed();
                eprintln!("  jit tx[{i}] error: {e:?}");
                jit_results.push((false, 0));
            }
        }
    }

    Ok(BlockResult {
        native_results,
        jit_results,
        native_dur,
        jit_dur,
    })
}

// ── Statistics ──────────────────────────────────────────────────────────────

struct BlockStats {
    block_number: u64,
    tx_count: usize,
    native_dur: Duration,
    jit_dur: Duration,
    total_native_gas: u64,
    total_jit_gas: u64,
    mismatches: usize,
}

impl BlockStats {
    fn speedup(&self) -> f64 {
        self.native_dur.as_secs_f64() / self.jit_dur.as_secs_f64()
    }

    fn print_summary(&self) {
        println!(
            "Block {} | {:>3} txs | native {:.2}ms | jit {:.2}ms | {:.2}x | gas n={} j={} | {}",
            self.block_number,
            self.tx_count,
            self.native_dur.as_secs_f64() * 1000.0,
            self.jit_dur.as_secs_f64() * 1000.0,
            self.speedup(),
            self.total_native_gas,
            self.total_jit_gas,
            if self.mismatches > 0 {
                format!("{} MISMATCH", self.mismatches)
            } else {
                "OK".into()
            },
        );
    }
}

// ── CLI + Main ──────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "bin_bench",
    about = "Multi-block JIT benchmark: Native vs revmc JIT"
)]
struct Args {
    /// Path to bench_data directory
    #[arg(long, default_value = "/home/ubuntu/sipeng/bench_data")]
    dir: String,

    /// First block number
    #[arg(long, default_value_t = 38004930)]
    start: u64,

    /// Last block number (inclusive, overrides --count)
    #[arg(long)]
    end: Option<u64>,

    /// Number of blocks to run (from --start)
    #[arg(long, default_value_t = 10)]
    count: u64,

    /// Step between sampled blocks (e.g. 1000 = every 1000th block)
    #[arg(long, default_value_t = 1000)]
    step: u64,

    /// Persistent AOT cache directory
    #[arg(long)]
    cache_dir: Option<String>,
}

fn main() {
    let args = Args::parse();
    let bench_dir = Path::new(&args.dir);
    let block_range: Vec<u64> = if let Some(end) = args.end {
        (args.start..=end).collect()
    } else {
        (0..args.count).map(|i| args.start + i * args.step).collect()
    };

    println!(
        "=== Bin JIT Benchmark: {} blocks (start={}, step={}) ===\n",
        block_range.len(), args.start, args.step,
    );

    // Phase 1: Load all blocks
    eprintln!("=== Loading blocks ===");
    let load_start = Instant::now();
    let loaders: Vec<BinLoader> = block_range
        .iter()
        .filter_map(|&bn| match BinLoader::new(bench_dir, bn) {
            Ok(l) => {
                eprintln!(
                    "  block {bn}: {} txs, {} accounts, {} codes",
                    l.tx_count(),
                    l.account_count(),
                    l.code_count()
                );
                Some(l)
            }
            Err(e) => {
                eprintln!("  skip block {bn}: {e}");
                None
            }
        })
        .collect();
    eprintln!(
        "  Loaded {} blocks in {:.2}s\n",
        loaders.len(),
        load_start.elapsed().as_secs_f64()
    );

    if loaders.is_empty() {
        eprintln!("No blocks loaded.");
        return;
    }

    // Phase 2: Collect unique bytecodes and compile
    eprintln!("=== Compiling contracts ===");
    let all_codes = collect_unique_bytecodes(&loaders);
    eprintln!("  Unique bytecodes across all blocks: {}", all_codes.len());

    let compile_start = Instant::now();
    let compiled = if let Some(ref cache_dir) = args.cache_dir {
        compile_all_contracts_with_cache(
            &all_codes,
            OptimizationLevel::Aggressive,
            Path::new(cache_dir),
        )
    } else {
        compile_all_contracts(&all_codes)
    };
    let compile_dur = compile_start.elapsed();
    eprintln!("  Compilation time: {:.2}s\n", compile_dur.as_secs_f64());

    // Phase 3: Execute blocks
    eprintln!("=== Benchmark: Native vs JIT ===");
    let mut all_stats: Vec<BlockStats> = Vec::new();
    let mut skipped = 0u64;

    for loader in &loaders {
        let block_result = match run_block(loader, &compiled.functions) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("FAIL block {}: {e}", loader.block_number());
                skipped += 1;
                continue;
            }
        };

        let mut mismatches = 0;
        for i in 0..loader.tx_count() {
            let (_, n_gas) = block_result.native_results[i];
            let (_, j_gas) = block_result.jit_results[i];
            if n_gas != j_gas {
                eprintln!(
                    "  GAS MISMATCH block {} tx[{i}]: native={n_gas} jit={j_gas}",
                    loader.block_number()
                );
                mismatches += 1;
            }
        }

        let stats = BlockStats {
            block_number: loader.block_number(),
            tx_count: loader.tx_count(),
            native_dur: block_result.native_dur,
            jit_dur: block_result.jit_dur,
            total_native_gas: block_result.native_results.iter().map(|(_, g)| g).sum(),
            total_jit_gas: block_result.jit_results.iter().map(|(_, g)| g).sum(),
            mismatches,
        };
        stats.print_summary();
        all_stats.push(stats);
    }

    // Phase 4: Aggregate
    if all_stats.is_empty() {
        println!("\nNo blocks completed.");
        return;
    }

    let total_txs: usize = all_stats.iter().map(|s| s.tx_count).sum();
    let total_native: Duration = all_stats.iter().map(|s| s.native_dur).sum();
    let total_jit: Duration = all_stats.iter().map(|s| s.jit_dur).sum();
    let total_native_gas: u64 = all_stats.iter().map(|s| s.total_native_gas).sum();
    let total_jit_gas: u64 = all_stats.iter().map(|s| s.total_jit_gas).sum();
    let total_mismatches: usize = all_stats.iter().map(|s| s.mismatches).sum();

    println!("\n========== Aggregate ==========");
    println!(
        "Blocks: {} ok, {skipped} skipped | Txs: {total_txs}",
        all_stats.len(),
    );
    println!(
        "Compile:   {:.2}s (one-time, shared across all blocks)",
        compile_dur.as_secs_f64(),
    );
    println!(
        "Native:    {:.2}ms ({:.2} ms/block)",
        total_native.as_secs_f64() * 1000.0,
        total_native.as_secs_f64() * 1000.0 / all_stats.len() as f64,
    );
    println!(
        "JIT:       {:.2}ms ({:.2} ms/block)",
        total_jit.as_secs_f64() * 1000.0,
        total_jit.as_secs_f64() * 1000.0 / all_stats.len() as f64,
    );
    println!(
        "Speedup:   {:.2}x (native/jit)",
        total_native.as_secs_f64() / total_jit.as_secs_f64(),
    );
    println!("Gas: native={total_native_gas}, jit={total_jit_gas}");
    if total_mismatches > 0 {
        println!(
            "WARNING: {total_mismatches} gas mismatches across {} blocks",
            all_stats.len()
        );
    } else {
        println!(
            "All {total_txs} txs across {} blocks gas match OK",
            all_stats.len()
        );
    }
}
