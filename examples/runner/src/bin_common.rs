// Shared infrastructure for bin_bench and frame_bench binaries.
//
// Contains binary data types, block/tx loading, JIT compilation,
// AOT caching, and EVM handler implementations.
//
// Included via #[path] from each binary; not all items are used by every consumer.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use op_revm::transaction::OpTransaction;
use op_revm::{DefaultOp, OpEvm, OpHaltReason, OpSpecId, OpTransactionError};
use revm::{
    bytecode::Bytecode,
    context::{CfgEnv, TxEnv},
    context_interface::result::{EVMError, ExecutionResult},
    context_interface::JournalTr,
    database::{CacheDB, EmptyDB},
    handler::{EvmTr, FrameResult, Handler, ItemOrResult},
    primitives::{hardfork::SpecId, Address, Bytes, HashMap as RevmHashMap, TxKind, B256, U256},
    state::AccountInfo,
    DatabaseCommit,
};
use revmc::{EvmCompiler, EvmLlvmBackend, OptimizationLevel};
use revmc_context::{EvmCompilerFn, RawEvmCompilerFn};
use serde::Deserialize;

#[path = "jit_lookup.rs"]
mod jit_lookup;

pub use jit_lookup::should_lookup_jit;

pub type OpCtx<DB> = op_revm::OpContext<DB>;
pub type BenchEvm = OpEvm<op_revm::OpContext<CacheDB<EmptyDB>>, ()>;
pub type BenchError = EVMError<core::convert::Infallible, OpTransactionError>;

#[derive(Debug, Clone, Copy, Default)]
pub struct JitDispatchStats {
    pub total_frames: u64,
    pub lookup_attempts: u64,
    pub lookup_hits: u64,
    pub lookup_misses: u64,
    pub skip_create: u64,
    pub skip_no_bytecode_address: u64,
    pub skip_empty_bytecode: u64,
}

static JIT_TOTAL_FRAMES: AtomicU64 = AtomicU64::new(0);
static JIT_LOOKUP_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
static JIT_LOOKUP_HITS: AtomicU64 = AtomicU64::new(0);
static JIT_LOOKUP_MISSES: AtomicU64 = AtomicU64::new(0);
static JIT_SKIP_CREATE: AtomicU64 = AtomicU64::new(0);
static JIT_SKIP_NO_BYTECODE_ADDRESS: AtomicU64 = AtomicU64::new(0);
static JIT_SKIP_EMPTY_BYTECODE: AtomicU64 = AtomicU64::new(0);
static JIT_DISPATCH_STATS_ENABLED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JitLookupDecision {
    Lookup,
    SkipCreate,
    SkipNoBytecodeAddress,
    SkipEmptyBytecode,
}

#[inline]
fn jit_lookup_decision(
    frame_is_create: bool,
    bytecode_address: Option<Address>,
    bytecode_is_empty: bool,
) -> JitLookupDecision {
    if should_lookup_jit(frame_is_create, bytecode_address, bytecode_is_empty) {
        JitLookupDecision::Lookup
    } else if frame_is_create {
        JitLookupDecision::SkipCreate
    } else if bytecode_address.is_none() {
        JitLookupDecision::SkipNoBytecodeAddress
    } else if bytecode_is_empty {
        JitLookupDecision::SkipEmptyBytecode
    } else {
        // All false negatives are handled above.
        unreachable!("inconsistent JIT lookup decision inputs")
    }
}

pub fn reset_jit_dispatch_stats() {
    JIT_TOTAL_FRAMES.store(0, Ordering::Relaxed);
    JIT_LOOKUP_ATTEMPTS.store(0, Ordering::Relaxed);
    JIT_LOOKUP_HITS.store(0, Ordering::Relaxed);
    JIT_LOOKUP_MISSES.store(0, Ordering::Relaxed);
    JIT_SKIP_CREATE.store(0, Ordering::Relaxed);
    JIT_SKIP_NO_BYTECODE_ADDRESS.store(0, Ordering::Relaxed);
    JIT_SKIP_EMPTY_BYTECODE.store(0, Ordering::Relaxed);
}

pub fn set_jit_dispatch_stats_enabled(enabled: bool) {
    JIT_DISPATCH_STATS_ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn read_jit_dispatch_stats() -> JitDispatchStats {
    JitDispatchStats {
        total_frames: JIT_TOTAL_FRAMES.load(Ordering::Relaxed),
        lookup_attempts: JIT_LOOKUP_ATTEMPTS.load(Ordering::Relaxed),
        lookup_hits: JIT_LOOKUP_HITS.load(Ordering::Relaxed),
        lookup_misses: JIT_LOOKUP_MISSES.load(Ordering::Relaxed),
        skip_create: JIT_SKIP_CREATE.load(Ordering::Relaxed),
        skip_no_bytecode_address: JIT_SKIP_NO_BYTECODE_ADDRESS.load(Ordering::Relaxed),
        skip_empty_bytecode: JIT_SKIP_EMPTY_BYTECODE.load(Ordering::Relaxed),
    }
}

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
        let mut snapshot: CacheSnapshot = bincode::deserialize(&snapshot_data)
            .map_err(|e| format!("deserialize states/{block_number}.bin: {e}"))?;

        // Re-analyze legacy code from original bytes: the snapshot's stored
        // jump tables predate the linked revm's metadata jump mask, and
        // execution must see the current (hardened) semantics.
        let reanalyze = |bytecode: &mut Bytecode| {
            if let Bytecode::LegacyAnalyzed(analyzed) = &*bytecode {
                *bytecode = Bytecode::new_legacy(analyzed.original_bytes());
            }
        };
        snapshot.codes.values_mut().for_each(reanalyze);
        for acc in snapshot.accounts.values_mut() {
            if let Some(code) = acc.info.as_mut().and_then(|info| info.code.as_mut()) {
                reanalyze(code);
            }
        }

        let txs_data = std::fs::read(&txs_path).map_err(|e| format!("read {:?}: {e}", txs_path))?;
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
    let collect_stats = JIT_DISPATCH_STATS_ENABLED.load(Ordering::Relaxed);
    let decision = jit_lookup_decision(
        frame.data.is_create(),
        frame.interpreter.input.bytecode_address,
        frame.interpreter.bytecode.is_empty(),
    );
    if collect_stats {
        JIT_TOTAL_FRAMES.fetch_add(1, Ordering::Relaxed);
        match decision {
            JitLookupDecision::Lookup => {
                JIT_LOOKUP_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
            }
            JitLookupDecision::SkipCreate => {
                JIT_SKIP_CREATE.fetch_add(1, Ordering::Relaxed);
                drop((ctx, frame_stack));
                return Ok(evm.frame_run()?);
            }
            JitLookupDecision::SkipNoBytecodeAddress => {
                JIT_SKIP_NO_BYTECODE_ADDRESS.fetch_add(1, Ordering::Relaxed);
                drop((ctx, frame_stack));
                return Ok(evm.frame_run()?);
            }
            JitLookupDecision::SkipEmptyBytecode => {
                JIT_SKIP_EMPTY_BYTECODE.fetch_add(1, Ordering::Relaxed);
                drop((ctx, frame_stack));
                return Ok(evm.frame_run()?);
            }
        }
    } else if decision != JitLookupDecision::Lookup {
        drop((ctx, frame_stack));
        return Ok(evm.frame_run()?);
    }

    let bytecode_hash = frame.interpreter.bytecode.get_or_calculate_hash();
    if let Some(&raw_fn) = functions.get(&bytecode_hash) {
        if collect_stats {
            JIT_LOOKUP_HITS.fetch_add(1, Ordering::Relaxed);
        }
        let f = EvmCompilerFn::new(raw_fn);
        let action = unsafe { f.call_with_interpreter(&mut frame.interpreter, ctx) };
        let result = frame.process_next_action::<_, BenchError>(ctx, action).inspect(|i| {
            if i.is_result() {
                frame.set_finished(true);
            }
        })?;
        Ok(result)
    } else {
        if collect_stats {
            JIT_LOOKUP_MISSES.fetch_add(1, Ordering::Relaxed);
        }
        drop((ctx, frame_stack));
        Ok(evm.frame_run()?)
    }
}

// ── Single-tx snapshot helpers ──────────────────────────────────────────────

/// Replay txs `[0..tx_index)` on the block's pre-state using `NativeHandler` to
/// reconstruct the exact database state right before `tx_index`.
/// `skip_deposits` matches the benchmark-script convention (skip tx_type 0x7e).
pub fn snapshot_before_tx(
    loader: &BinLoader,
    tx_index: usize,
    skip_deposits: bool,
) -> Result<CacheDB<EmptyDB>, String> {
    let chain_id = loader.raw_txs().first().and_then(|tx| tx.chain_id);
    let cfg = build_op_cfg(chain_id);
    let dummy_tx = OpTransaction::builder().build_fill();

    let mut evm: BenchEvm = {
        let ctx =
            OpCtx::<EmptyDB>::op().with_cfg(cfg).with_db(loader.build_cache_db()).with_tx(dummy_tx);
        op_revm::OpEvm::new(ctx, ())
    };

    for (i, tx_bin) in loader.raw_txs().iter().enumerate() {
        if i == tx_index {
            break;
        }
        if skip_deposits && tx_bin.tx_type == 0x7e {
            continue;
        }
        evm.0.ctx.tx = build_op_tx(tx_bin);
        let mut handler = NativeHandler;
        handler.run(&mut evm).map_err(|e| format!("replay tx[{i}] failed: {e:?}"))?;
        // Handler::run leaves each tx's state changes in the journal; fold
        // them into the backing CacheDB, otherwise the returned snapshot is
        // just the block's initial state with all replay effects discarded.
        let state = evm.0.ctx.journaled_state.finalize();
        evm.0.ctx.journaled_state.database.commit(state);
    }

    Ok(evm.0.ctx.journaled_state.database.clone())
}

/// Build a fresh `BenchEvm` by cloning `snapshot` as the backing DB and seeding
/// `ctx.tx` with `tx_bin`. Caller typically installs a custom handler before
/// calling `.run(&mut evm)`.
pub fn single_tx_evm(
    snapshot: &CacheDB<EmptyDB>,
    chain_id: Option<u64>,
    tx_bin: &TxBin,
) -> BenchEvm {
    let cfg = build_op_cfg(chain_id);
    let dummy_tx = OpTransaction::builder().build_fill();
    let mut evm: BenchEvm = {
        let ctx = OpCtx::<EmptyDB>::op().with_cfg(cfg).with_db(snapshot.clone()).with_tx(dummy_tx);
        op_revm::OpEvm::new(ctx, ())
    };
    evm.0.ctx.tx = build_op_tx(tx_bin);
    evm
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

/// Returns true if a bytecode should be skipped during JIT compilation.
///
/// Bytecodes starting with 0xEF are either EIP-7702 delegated code (resolved at the
/// frame level, not executed directly) or legacy INVALID — compiling either is incorrect.
fn should_skip_compilation(bc: &Bytecode) -> bool {
    bc.original_byte_slice().first() == Some(&0xEF)
}

pub fn compile_all_contracts(code_values: &HashMap<B256, Bytecode>) -> CompiledContracts {
    let mut contracts: Vec<(B256, &Bytecode)> = code_values
        .iter()
        .filter(|(_, bc)| !bc.is_empty() && !should_skip_compilation(bc))
        .map(|(h, bc)| (*h, bc))
        .collect();
    contracts.sort_by_key(|(_, bc)| std::cmp::Reverse(bc.original_byte_slice().len()));

    let empty_count = code_values.len() - contracts.len();
    let n_threads = std::thread::available_parallelism().map(|n| n.get().min(16)).unwrap_or(4);
    eprintln!(
        "  Using {n_threads} threads for {}/{} contracts ({empty_count} empty)",
        contracts.len(),
        code_values.len()
    );

    let mut assignments: Vec<Vec<(B256, &Bytecode)>> = vec![vec![]; n_threads];
    for (i, contract) in contracts.into_iter().enumerate() {
        assignments[i % n_threads].push(contract);
    }

    let thread_results: Vec<(HashMap<B256, RawEvmCompilerFn>, usize)> = std::thread::scope(|s| {
        let handles: Vec<_> = assignments
            .into_iter()
            .enumerate()
            .map(|(tid, chunk)| {
                s.spawn(move || {
                    let context =
                        Box::leak(Box::new(revmc::llvm::inkwell::context::Context::create()));
                    let backend =
                        EvmLlvmBackend::new(context, false, OptimizationLevel::Aggressive)
                            .expect("LLVM backend");
                    let compiler: &'static mut EvmCompiler<EvmLlvmBackend<'static>> =
                        Box::leak(Box::new(EvmCompiler::new(backend)));

                    let mut pending = Vec::new();
                    let mut skipped = 0usize;
                    for (hash, bytecode) in &chunk {
                        let name = format!("c_{}", hex::encode(&hash.as_slice()[..8]));
                        match compiler.translate(&name, bytecode.original_byte_slice(), ETH_SPEC) {
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
                    eprintln!("  [T{tid}] done: {}/{} compiled", functions.len(), chunk.len());
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

    CompiledContracts { functions: Arc::new(all_functions), _libraries: Vec::new() }
}

// ── AOT cache + tx-index (split modules) ───────────────────────────────────

#[path = "aot_cache.rs"]
mod aot_cache;
#[allow(unused_imports)]
pub use aot_cache::*;

#[path = "tx_index.rs"]
mod tx_index;
#[allow(unused_imports)]
pub use tx_index::*;

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Create a fresh EVM with a CacheDB from the loader.
pub fn make_evm(loader: &BinLoader, chain_id: Option<u64>) -> BenchEvm {
    let cfg = build_op_cfg(chain_id);
    let db = loader.build_cache_db();
    let dummy_tx = OpTransaction::builder().build_fill();
    let ctx = OpCtx::<EmptyDB>::op().with_cfg(cfg).with_db(db).with_tx(dummy_tx);
    OpEvm::new(ctx, ())
}

/// Same as [`make_evm`] but with an inspector attached. Diagnostics only —
/// the step hook fires on every opcode, so this must never be used in a
/// timed measurement.
pub fn make_evm_with_inspector<I>(
    loader: &BinLoader,
    chain_id: Option<u64>,
    inspector: I,
) -> OpEvm<op_revm::OpContext<CacheDB<EmptyDB>>, I> {
    let cfg = build_op_cfg(chain_id);
    let db = loader.build_cache_db();
    let dummy_tx = OpTransaction::builder().build_fill();
    let ctx = OpCtx::<EmptyDB>::op().with_cfg(cfg).with_db(db).with_tx(dummy_tx);
    OpEvm::new(ctx, inspector)
}

#[cfg(test)]
mod tests {
    use super::should_lookup_jit;
    use revm::primitives::Address;

    #[test]
    fn skips_create_frames() {
        let addr = Address::new([0x11; 20]);
        assert!(!should_lookup_jit(true, Some(addr), false));
    }

    #[test]
    fn skips_eoa_or_empty_bytecode_frames() {
        let addr = Address::new([0x22; 20]);
        assert!(!should_lookup_jit(false, None, false));
        assert!(!should_lookup_jit(false, Some(addr), true));
    }

    #[test]
    fn looks_up_only_for_non_create_contract_frames() {
        let addr = Address::new([0x33; 20]);
        assert!(should_lookup_jit(false, Some(addr), false));
    }
}
