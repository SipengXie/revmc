// Shared infrastructure for bin_bench and frame_bench binaries.
//
// Contains binary data types, block/tx loading, JIT compilation,
// AOT caching, and EVM handler implementations.
//
// Included via #[path] from each binary; not all items are used by every consumer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
use revmc_context::{EvmCompilerFn, RawEvmCompilerFn};
use serde::Deserialize;

pub type OpCtx<DB> = op_revm::OpContext<DB>;
pub type BenchEvm = OpEvm<op_revm::OpContext<CacheDB<EmptyDB>>, ()>;
pub type BenchError = EVMError<core::convert::Infallible, OpTransactionError>;

// ── Spec Constants ──────────────────────────────────────────────────────────

pub const OP_SPEC: OpSpecId = OpSpecId::ISTHMUS;
pub const ETH_SPEC: SpecId = OP_SPEC.into_eth_spec();

// ── Binary Data Types ───────────────────────────────────────────────────────

#[derive(Clone, Deserialize)]
pub struct AccountSnapshot {
    pub info: Option<AccountInfo>,
    pub storage: RevmHashMap<U256, U256>,
}

#[derive(Clone, Deserialize)]
pub struct CacheSnapshot {
    pub block_number: u64,
    #[allow(dead_code)]
    pub has_state_clear: bool,
    pub accounts: RevmHashMap<Address, AccountSnapshot>,
    pub codes: RevmHashMap<B256, Bytecode>,
}

#[derive(Deserialize)]
pub struct BlockBin {
    #[allow(dead_code)]
    pub block_number: u64,
    pub txs: Vec<TxBin>,
}

#[derive(Deserialize)]
pub struct TxBin {
    pub tx_type: u8,
    pub caller: [u8; 20],
    pub gas_limit: u64,
    pub gas_price: u128,
    pub to: Option<[u8; 20]>,
    pub value: [u8; 32],
    pub data: Vec<u8>,
    pub nonce: u64,
    pub chain_id: Option<u64>,
    #[allow(dead_code)]
    pub access_list: Vec<AccessListItemBin>,
    #[allow(dead_code)]
    pub gas_priority_fee: Option<u128>,
    #[allow(dead_code)]
    pub max_fee_per_blob_gas: u128,
    pub source_hash: Option<[u8; 32]>,
    pub mint: Option<[u8; 32]>,
    pub is_system_tx: bool,
}

#[derive(Deserialize)]
pub struct AccessListItemBin {
    #[allow(dead_code)]
    pub address: [u8; 20],
    #[allow(dead_code)]
    pub storage_keys: Vec<[u8; 32]>,
}

impl TxBin {
    pub fn to_tx_env(&self) -> TxEnv {
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

pub struct BinLoader {
    pub snapshot: CacheSnapshot,
    pub block_bin: BlockBin,
}

impl BinLoader {
    pub fn new(bench_dir: &Path, block_number: u64) -> Result<Self, String> {
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

    pub fn block_number(&self) -> u64 {
        self.snapshot.block_number
    }

    pub fn tx_count(&self) -> usize {
        self.block_bin.txs.len()
    }

    pub fn account_count(&self) -> usize {
        self.snapshot.accounts.len()
    }

    pub fn code_count(&self) -> usize {
        self.snapshot.codes.len()
    }

    pub fn raw_txs(&self) -> &[TxBin] {
        &self.block_bin.txs
    }

    pub fn code_values(&self) -> &RevmHashMap<B256, Bytecode> {
        &self.snapshot.codes
    }

    pub fn build_cache_db(&self) -> CacheDB<EmptyDB> {
        let mut db = CacheDB::new(EmptyDB::new());
        for (addr, acc) in &self.snapshot.accounts {
            if let Some(info) = &acc.info {
                let mut acc_info = info.clone();
                if acc_info.code.is_none() {
                    acc_info.code = self.snapshot.codes.get(&info.code_hash).cloned();
                }
                db.insert_account_info(*addr, acc_info);
                if !acc.storage.is_empty() {
                    db.replace_account_storage(*addr, acc.storage.clone()).ok();
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

pub fn build_op_cfg(chain_id: Option<u64>) -> CfgEnv<OpSpecId> {
    let mut cfg = CfgEnv::new_with_spec(OP_SPEC);
    cfg.tx_chain_id_check = false;
    if let Some(id) = chain_id {
        cfg.chain_id = id;
    }
    cfg
}

pub fn build_op_tx(tx: &TxBin) -> OpTransaction<TxEnv> {
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

pub fn extract_gas(result: &ExecutionResult<OpHaltReason>) -> (bool, u64) {
    match result {
        ExecutionResult::Success { gas_used, .. } => (true, *gas_used),
        ExecutionResult::Revert { gas_used, .. } => (false, *gas_used),
        ExecutionResult::Halt { gas_used, .. } => (false, *gas_used),
    }
}

// ── Handlers ────────────────────────────────────────────────────────────────

pub struct NativeHandler;

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

pub struct JitHandler {
    pub functions: Arc<HashMap<B256, RawEvmCompilerFn>>,
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
            let call_or_result = run_jit_or_native(evm, &self.functions)?;
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

/// Execute the current frame using JIT if available, otherwise fall back to the interpreter.
pub fn run_jit_or_native(
    evm: &mut BenchEvm,
    functions: &HashMap<B256, RawEvmCompilerFn>,
) -> Result<ItemOrResult<revm::interpreter::interpreter_action::FrameInit, FrameResult>, BenchError>
{
    let (ctx, frame_stack) = (&mut evm.0.ctx, &mut evm.0.frame_stack);
    let frame = frame_stack.get();
    let bytecode_hash = frame.interpreter.bytecode.get_or_calculate_hash();
    if let Some(&raw_fn) = functions.get(&bytecode_hash) {
        let f = EvmCompilerFn::new(raw_fn);
        let action = unsafe { f.call_with_interpreter(&mut frame.interpreter, ctx) };
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

// ── Compilation ─────────────────────────────────────────────────────────────

pub struct CompiledContracts {
    pub functions: Arc<HashMap<B256, RawEvmCompilerFn>>,
    pub _libraries: Vec<libloading::Library>,
}

/// Collect unique bytecodes across multiple BinLoaders, deduplicated by code hash.
pub fn collect_unique_bytecodes(loaders: &[BinLoader]) -> HashMap<B256, Bytecode> {
    let mut all_codes: HashMap<B256, Bytecode> = HashMap::new();
    for loader in loaders {
        for (hash, bytecode) in loader.code_values() {
            all_codes.entry(*hash).or_insert_with(|| bytecode.clone());
        }
    }
    all_codes
}

pub fn compile_all_contracts(code_values: &HashMap<B256, Bytecode>) -> CompiledContracts {
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

pub fn compile_all_contracts_with_cache(
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

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Create a fresh EVM with a CacheDB from the loader.
pub fn make_evm(loader: &BinLoader, chain_id: Option<u64>) -> BenchEvm {
    let cfg = build_op_cfg(chain_id);
    let db = loader.build_cache_db();
    let dummy_tx = OpTransaction::builder().build_fill();
    let ctx = OpCtx::<EmptyDB>::op()
        .with_cfg(cfg)
        .with_db(db)
        .with_tx(dummy_tx);
    OpEvm::new(ctx, ())
}
