//! JSONL benchmark: revmc JIT vs native revm interpreter.
//!
//! Loads Base mainnet block data from JSONL format and runs each transaction
//! in both plain (interpreter) and JIT (revmc-compiled) modes, comparing
//! gas and performance.
//!
//! Usage: jsonl_bench --path <path-to-jsonl-file>

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[path = "../jit_lookup.rs"]
mod jit_lookup;

use op_revm::{DefaultOp, OpEvm, OpHaltReason, OpSpecId, OpTransactionError};
use op_revm::transaction::OpTransaction;
use revm::{
    bytecode::Bytecode,
    context::{CfgEnv, Context, TxEnv},
    context_interface::result::EVMError,
    database::{CacheDB, EmptyDB},
    handler::{EvmTr, FrameResult, Handler, ItemOrResult},
    primitives::{
        hardfork::SpecId, Address, Bytes, HashMap as RevmHashMap, StorageKey, StorageValue,
        TxKind, B256, U256,
    },
    state::AccountInfo,

};
use revmc::{EvmCompiler, EvmLlvmBackend, OptimizationLevel};
use revmc_builtins as _;
use revmc_context::{EvmCompilerFn, RawEvmCompilerFn};
use serde::Deserialize;
use jit_lookup::should_lookup_jit;

// ── Spec Constants ──────────────────────────────────────────────────────────
// Single source of truth: OpSpecId for execution, derived SpecId for JIT compilation.
const OP_SPEC: OpSpecId = OpSpecId::ISTHMUS;
const ETH_SPEC: SpecId = OP_SPEC.into_eth_spec(); // PRAGUE

// ── JSONL Serde Structs ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct CodeValuesLine {
    #[allow(dead_code)]
    block: u64,
    #[serde(rename = "type")]
    line_type: String,
    items: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct TxRecordLine {
    #[allow(dead_code)]
    block: u64,
    tx_index: u64,
    #[allow(dead_code)]
    gas_limit: u64,
    #[allow(dead_code)]
    exec_time_ns: u64,
    tx: TxData,
    read: ReadSet,
    #[allow(dead_code)]
    write: WriteSet,
}

#[derive(Debug, Deserialize)]
struct TxData {
    tx_type: u8,
    caller: String,
    gas_limit: u64,
    nonce: u64,
    value: String,
    input: String,
    kind: String,
    to: Option<String>,
    chain_id: u64,
    gas_price: u64,
    #[allow(dead_code)]
    max_fee_per_gas: u64,
    #[allow(dead_code)]
    access_list_len: usize,
    #[allow(dead_code)]
    blob_versioned_hashes: Vec<String>,
    #[allow(dead_code)]
    max_fee_per_blob_gas: u64,
}

#[derive(Debug, Deserialize)]
struct ReadSet {
    #[allow(dead_code)]
    accounts: Vec<String>,
    account_values: Vec<AccountValue>,
    #[allow(dead_code)]
    storage: Vec<(String, String)>,
    storage_values: Vec<StorageValueEntry>,
}

#[derive(Debug, Deserialize)]
struct WriteSet {
    #[allow(dead_code)]
    accounts: Vec<String>,
    #[allow(dead_code)]
    storage: Vec<(String, String)>,
}

#[derive(Debug, Deserialize)]
struct AccountValue {
    addr: String,
    nonce: Option<u64>,
    balance: Option<String>,
    code_hash: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StorageValueEntry {
    addr: String,
    slot: String,
    value: String,
}

// ── JSONL Loading ───────────────────────────────────────────────────────────

fn load_jsonl(path: &str) -> (HashMap<B256, Bytecode>, Vec<TxRecordLine>) {
    let file = File::open(path).expect("failed to open JSONL file");
    let reader = BufReader::new(file);
    let mut lines = reader.lines();

    // Line 1: code_values
    let first_line = lines.next().expect("empty file").expect("read error");
    let code_line: CodeValuesLine =
        serde_json::from_str(&first_line).expect("parse code_values failed");
    assert_eq!(code_line.line_type, "code_values");
    let code_values = parse_code_values(&code_line.items);

    // Lines 2+: tx records
    let mut tx_records = Vec::new();
    for line in lines {
        let line = line.expect("read error");
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<TxRecordLine>(&line) {
            Ok(record) => tx_records.push(record),
            Err(e) => eprintln!("  WARN: skip malformed tx line: {e}"),
        }
    }

    (code_values, tx_records)
}

fn parse_code_values(items: &HashMap<String, String>) -> HashMap<B256, Bytecode> {
    let mut result = HashMap::new();
    for (hash_str, bytecode_str) in items {
        let hash = parse_b256_hex(hash_str);
        let bytecode = extract_bytecode(bytecode_str);
        result.insert(hash, bytecode);
    }
    result
}

fn extract_bytecode(debug_str: &str) -> Bytecode {
    // Format: "LegacyAnalyzed(LegacyAnalyzedBytecode { bytecode: 0x... })" or "0x..."
    let hex_str = if debug_str.contains("bytecode:") {
        let start = debug_str.find("0x").unwrap_or_else(|| {
            panic!(
                "no 0x prefix in bytecode: {}",
                &debug_str[..50.min(debug_str.len())]
            )
        });
        let end = debug_str[start..]
            .find(|c: char| !c.is_ascii_hexdigit() && c != 'x')
            .map(|i| start + i)
            .unwrap_or(debug_str.len());
        &debug_str[start..end]
    } else if debug_str.starts_with("0x") {
        debug_str
    } else {
        panic!(
            "unknown bytecode format: {}",
            &debug_str[..50.min(debug_str.len())]
        )
    };
    let bytes = hex::decode(hex_str.trim_start_matches("0x")).expect("invalid bytecode hex");
    Bytecode::new_raw(Bytes::from(bytes))
}

// ── CacheDB & TxEnv Building ────────────────────────────────────────────────

fn build_cache_db(tx: &TxRecordLine, code_values: &HashMap<B256, Bytecode>) -> CacheDB<EmptyDB> {
    let mut db = CacheDB::new(EmptyDB::new());

    // Group storage by address
    let mut storage_by_addr: HashMap<String, RevmHashMap<StorageKey, StorageValue>> =
        HashMap::new();
    for sv in &tx.read.storage_values {
        let slot = parse_u256_hex(&sv.slot);
        let value = parse_u256_hex(&sv.value);
        storage_by_addr
            .entry(sv.addr.clone())
            .or_default()
            .insert(slot, value);
    }

    for acc in &tx.read.account_values {
        let addr: Address = acc.addr.parse().expect("invalid address");
        let (nonce, balance, code_hash) = match (&acc.nonce, &acc.balance, &acc.code_hash) {
            (Some(n), Some(b), Some(ch)) => (*n, parse_u256_hex(b), parse_b256_hex(ch)),
            _ => continue, // non-existent account
        };

        let bytecode = code_values.get(&code_hash).cloned();
        let info = AccountInfo {
            balance,
            nonce,
            code_hash,
            code: bytecode,
        };
        db.insert_account_info(addr, info);

        if let Some(storage) = storage_by_addr.remove(&acc.addr) {
            db.replace_account_storage(addr, storage)
                .expect("storage insert failed");
        }
    }

    db
}

fn build_tx_env(tx: &TxRecordLine) -> OpTransaction<TxEnv> {
    let caller: Address = tx.tx.caller.parse().expect("invalid caller");
    let value = parse_u256_hex(&tx.tx.value);
    let data = Bytes::from(
        hex::decode(tx.tx.input.trim_start_matches("0x")).unwrap_or_default(),
    );
    let kind = match tx.tx.kind.as_str() {
        "call" => {
            let to: Address = tx
                .tx
                .to
                .as_ref()
                .expect("call tx must have to")
                .parse()
                .expect("invalid to address");
            TxKind::Call(to)
        }
        "create" => TxKind::Create,
        other => panic!("unknown tx kind: {other}"),
    };

    let base = TxEnv {
        tx_type: tx.tx.tx_type,
        caller,
        gas_limit: tx.tx.gas_limit,
        gas_price: tx.tx.gas_price as u128,
        kind,
        value,
        data,
        nonce: tx.tx.nonce,
        chain_id: Some(tx.tx.chain_id),
        access_list: Default::default(),
        gas_priority_fee: None,
        blob_hashes: Vec::new(),
        max_fee_per_blob_gas: 0,
        authorization_list: Vec::new(),
    };
    let mut op_tx = OpTransaction::new(base);
    op_tx.enveloped_tx = Some(Bytes::new());
    op_tx
}

// ── Contract Compilation (parallel) ──────────────────────────────────────────

struct CompiledContracts {
    functions: Arc<HashMap<B256, RawEvmCompilerFn>>,
    _libraries: Vec<libloading::Library>,
}


struct CacheArtifacts {
    key: String,
    symbol: String,
    object: PathBuf,
    library: PathBuf,
}

fn sanitize_cache_component(input: &str) -> String {
    input
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn opt_cache_tag(opt: OptimizationLevel) -> &'static str {
    match opt {
        OptimizationLevel::None => "o0",
        OptimizationLevel::Less => "o1",
        OptimizationLevel::Default => "o2",
        OptimizationLevel::Aggressive => "o3",
    }
}

fn cache_artifacts(cache_dir: &Path, hash: &B256, opt: OptimizationLevel) -> CacheArtifacts {
    let hash_hex = hex::encode(hash);
    let spec_tag = sanitize_cache_component(&format!("{ETH_SPEC:?}"));
    let key = format!("{hash_hex}__{spec_tag}__{}", opt_cache_tag(opt));
    let stem = cache_dir.join(&key);
    let object = stem.with_extension("o");
    let library = stem.with_extension(std::env::consts::DLL_EXTENSION);
    let symbol = format!("c_{hash_hex}");
    CacheArtifacts {
        key,
        symbol,
        object,
        library,
    }
}

fn load_cached_symbol(
    artifacts: &CacheArtifacts,
) -> Result<(RawEvmCompilerFn, libloading::Library), String> {
    let library = unsafe { libloading::Library::new(&artifacts.library) }
        .map_err(|e| format!("dlopen {} failed: {e}", artifacts.library.display()))?;
    let symbol = unsafe { library.get::<RawEvmCompilerFn>(artifacts.symbol.as_bytes()) }
        .map_err(|e| {
            format!(
                "dlsym {} failed in {}: {e}",
                artifacts.symbol,
                artifacts.library.display()
            )
        })?;
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
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create cache dir {} failed: {e}", parent.display()))?;
    }

    let context = Box::leak(Box::new(revmc::llvm::inkwell::context::Context::create()));
    let backend = EvmLlvmBackend::new(context, true, opt)
        .map_err(|e| format!("AOT backend init failed ({}): {e}", artifacts.key))?;
    let mut compiler = EvmCompiler::new(backend);
    compiler
        .translate(&artifacts.symbol, bytecode.original_byte_slice(), ETH_SPEC)
        .map_err(|e| format!("translate {} failed: {e}", artifacts.key))?;
    compiler
        .write_object_to_file(&artifacts.object)
        .map_err(|e| format!("write object {} failed: {e}", artifacts.object.display()))?;

    revmc::Linker::new()
        .link(&artifacts.library, [&artifacts.object])
        .map_err(|e| format!("link {} failed: {e}", artifacts.library.display()))?;
    Ok(())
}

fn compile_contract_jit_fallback(
    hash: B256,
    bytecode: &Bytecode,
    opt: OptimizationLevel,
) -> Option<RawEvmCompilerFn> {
    let symbol = format!("c_{}", hex::encode(hash));
    let context = Box::leak(Box::new(revmc::llvm::inkwell::context::Context::create()));
    let backend = EvmLlvmBackend::new(context, false, opt).ok()?;
    let compiler: &'static mut EvmCompiler<EvmLlvmBackend<'static>> =
        Box::leak(Box::new(EvmCompiler::new(backend)));
    let func_id = compiler.translate(&symbol, bytecode.original_byte_slice(), ETH_SPEC).ok()?;
    unsafe { compiler.jit_function(func_id).ok().map(|f| f.into_inner()) }
}


fn compile_all_contracts_with_cache(
    code_values: &HashMap<B256, Bytecode>,
    opt: OptimizationLevel,
    cache_dir: &Path,
) -> CompiledContracts {
    std::fs::create_dir_all(cache_dir)
        .unwrap_or_else(|e| panic!("failed to create cache dir {}: {e}", cache_dir.display()));

    // Collect non-empty contracts, sorted by size descending for load balancing
    let mut contracts: Vec<(B256, &Bytecode)> = code_values
        .iter()
        .filter(|(_, bc)| !bc.is_empty())
        .map(|(h, bc)| (*h, bc))
        .collect();
    contracts.sort_by_key(|(_, bc)| std::cmp::Reverse(bc.original_byte_slice().len()));

    // Phase 1: Partition by cache status
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

    // Phase 2: Load cached contracts
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
                eprintln!("  Cache stale for {}..: {e}", &hex::encode(hash)[..16]);
                let bytecode = code_values.get(&hash).unwrap();
                to_compile.push((hash, bytecode, artifacts));
            }
        }
    }

    let n_threads = std::thread::available_parallelism()
        .map(|n| n.get().min(16))
        .unwrap_or(4);
    eprintln!(
        "  Persistent cache: dir={} hits={} to_compile={} threads={}",
        cache_dir.display(),
        hits,
        to_compile.len(),
        n_threads,
    );

    if to_compile.is_empty() {
        return CompiledContracts {
            functions: Arc::new(functions),
            _libraries: libraries,
        };
    }

    // Phase 3: Parallel compile cache misses
    // Round-robin assignment (already sorted by size desc) for even load distribution
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
                        let total = chunk.len();
                        let mut results = Vec::with_capacity(total);
                        for (hash, bytecode, artifacts) in chunk {
                            // Try AOT: compile → write .o → link .so → dlopen
                            if let Err(e) = compile_contract_to_cache(bytecode, opt, &artifacts) {
                                eprintln!(
                                    "  [T{tid}] AOT failed {}..: {e}",
                                    &hex::encode(hash)[..16]
                                );
                                if let Some(f) = compile_contract_jit_fallback(hash, bytecode, opt)
                                {
                                    results.push((hash, f, None));
                                }
                                continue;
                            }
                            match load_cached_symbol(&artifacts) {
                                Ok((f, lib)) => results.push((hash, f, Some(lib))),
                                Err(e) => {
                                    eprintln!(
                                        "  [T{tid}] cache load failed {}..: {e}",
                                        &hex::encode(hash)[..16]
                                    );
                                    if let Some(f) =
                                        compile_contract_jit_fallback(hash, bytecode, opt)
                                    {
                                        results.push((hash, f, None));
                                    }
                                }
                            }
                        }
                        eprintln!("  [T{tid}] done: {}/{total} compiled", results.len());
                        results
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

    // Phase 4: Merge
    let mut jit_fallbacks = 0usize;
    for thread_result in thread_results {
        for (hash, f, lib) in thread_result {
            functions.insert(hash, f);
            if let Some(lib) = lib {
                libraries.push(lib);
            } else {
                jit_fallbacks += 1;
            }
        }
    }

    eprintln!(
        "  Total: {} compiled (hits={} jit_fallbacks={})",
        functions.len(),
        hits,
        jit_fallbacks,
    );

    CompiledContracts {
        functions: Arc::new(functions),
        _libraries: libraries,
    }
}

fn compile_all_contracts(code_values: &HashMap<B256, Bytecode>) -> CompiledContracts {
    // Collect non-empty contracts, sorted by bytecode size descending for load balancing
    let mut contracts: Vec<(B256, &Bytecode)> = code_values
        .iter()
        .filter(|(_, bc)| !bc.is_empty())
        .map(|(h, bc)| (*h, bc))
        .collect();
    contracts.sort_by(|a, b| {
        b.1.original_byte_slice()
            .len()
            .cmp(&a.1.original_byte_slice().len())
    });

    let empty_count = code_values.len() - contracts.len();
    let n_threads = std::thread::available_parallelism()
        .map(|n| n.get().min(16))
        .unwrap_or(4);
    eprintln!("  Using {n_threads} threads for {}/{} contracts ({empty_count} empty)",
        contracts.len(), code_values.len());

    // Round-robin assignment for even load distribution (largest contracts spread across threads)
    let mut assignments: Vec<Vec<(B256, &Bytecode)>> = vec![vec![]; n_threads];
    for (i, contract) in contracts.into_iter().enumerate() {
        assignments[i % n_threads].push(contract);
    }

    // Parallel compile: each thread gets its own LLVM Context + Compiler
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
                        eprintln!("  [T{tid}] done: {}/{} compiled", functions.len(), chunk.len());
                        (functions, skipped)
                    })
                })
                .collect();

            handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect()
        });

    // Merge results from all threads
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

// ── Handlers ────────────────────────────────────────────────────────────────

type BenchEvm = OpEvm<op_revm::OpContext<CacheDB<EmptyDB>>, ()>;
type BenchError = EVMError<core::convert::Infallible, OpTransactionError>;

/// Standard exec loop using the native interpreter (no JIT).
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

/// JIT exec loop: looks up compiled functions by bytecode hash, falls back to interpreter.
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
                let (ctx, frame_stack) = (&mut evm.0.ctx, &mut evm.0.frame_stack);
                let frame = frame_stack.get();
                if !should_lookup_jit(
                    frame.data.is_create(),
                    frame.interpreter.input.bytecode_address,
                    frame.interpreter.bytecode.is_empty(),
                ) {
                    drop((ctx, frame_stack));
                    evm.frame_run()?
                } else {
                    let bytecode_hash = frame.interpreter.bytecode.get_or_calculate_hash();

                    if let Some(&raw_fn) = self.functions.get(&bytecode_hash) {
                        let f = EvmCompilerFn::new(raw_fn);
                        let action =
                            unsafe { f.call_with_interpreter(&mut frame.interpreter, ctx) };
                        frame
                            .process_next_action::<_, BenchError>(ctx, action)
                            .inspect(|i| {
                                if i.is_result() {
                                    frame.set_finished(true);
                                }
                            })?
                    } else {
                        // Fall back to interpreter for non-JIT contracts
                        drop((ctx, frame_stack));
                        evm.frame_run()?
                    }
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

// ── Execution Functions ─────────────────────────────────────────────────────

fn execute_plain(db: CacheDB<EmptyDB>, tx: OpTransaction<TxEnv>) -> Result<(u64, Duration), String> {
    let mut cfg: CfgEnv<OpSpecId> = CfgEnv::new_with_spec(OP_SPEC);
    cfg.chain_id = tx.base.chain_id.unwrap_or(8453);
    cfg.tx_chain_id_check = false;

    let ctx = Context::op()
        .with_db(db)
        .with_cfg(cfg)
        .with_tx(tx);
    let mut evm: OpEvm<_, ()> = OpEvm::new(ctx, ());

    let mut handler = NativeHandler;
    let start = Instant::now();
    let exec_result = handler.run(&mut evm).map_err(|e| format!("{e:?}"))?;
    let elapsed = start.elapsed();
    Ok((exec_result.gas_used(), elapsed))
}

fn execute_jit(
    db: CacheDB<EmptyDB>,
    tx: OpTransaction<TxEnv>,
    functions: &Arc<HashMap<B256, RawEvmCompilerFn>>,
) -> Result<(u64, Duration), String> {
    let mut cfg: CfgEnv<OpSpecId> = CfgEnv::new_with_spec(OP_SPEC);
    cfg.chain_id = tx.base.chain_id.unwrap_or(8453);
    cfg.tx_chain_id_check = false;

    let ctx = Context::op()
        .with_db(db)
        .with_cfg(cfg)
        .with_tx(tx);
    let mut evm: OpEvm<_, ()> = OpEvm::new(ctx, ());

    let mut handler = JitHandler {
        functions: functions.clone(),
    };
    let start = Instant::now();
    let exec_result = handler.run(&mut evm).map_err(|e| format!("{e:?}"))?;
    let elapsed = start.elapsed();
    Ok((exec_result.gas_used(), elapsed))
}

// ── Hex Parsing Helpers ─────────────────────────────────────────────────────

fn parse_u256_hex(s: &str) -> U256 {
    let trimmed = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    if trimmed.is_empty() {
        U256::ZERO
    } else {
        U256::from_str_radix(trimmed, 16).unwrap_or_default()
    }
}

fn parse_b256_hex(s: &str) -> B256 {
    let trimmed = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    // Handle short hashes by left-padding with zeros
    let padded = if trimmed.len() < 64 {
        format!("{:0>64}", trimmed)
    } else {
        trimmed.to_string()
    };
    let bytes = hex::decode(&padded).expect("invalid B256 hex");
    B256::from_slice(&bytes)
}

// ── Statistics & Output ─────────────────────────────────────────────────────

struct TxResult {
    tx_index: u64,
    gas: u64,
    native_us: f64,
    jit_us: f64,
}

fn print_results(results: &[TxResult], skipped: usize) {
    println!("\n  tx# |  Native(us) |    JIT(us) | JIT/Nat |        Gas");
    println!("------+-------------+------------+---------+-----------");

    let mut speedups: Vec<f64> = Vec::new();
    let mut total_native = Duration::ZERO;
    let mut total_jit = Duration::ZERO;
    let mut jit_faster_count = 0usize;

    for r in results {
        let ratio = if r.jit_us > 0.0 {
            r.native_us / r.jit_us
        } else {
            f64::INFINITY
        };
        speedups.push(ratio);
        total_native += Duration::from_secs_f64(r.native_us / 1_000_000.0);
        total_jit += Duration::from_secs_f64(r.jit_us / 1_000_000.0);
        if ratio > 1.0 {
            jit_faster_count += 1;
        }
        println!(
            " {:>4} | {:>11.1} | {:>10.1} | {:>6.2}x | {:>9}",
            r.tx_index, r.native_us, r.jit_us, ratio, r.gas
        );
    }

    if results.is_empty() {
        println!("\nNo transactions executed.");
        return;
    }

    speedups.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = speedups[speedups.len() / 2];
    let min_s = speedups[0];
    let max_s = speedups[speedups.len() - 1];
    let aggregate = total_native.as_secs_f64() / total_jit.as_secs_f64();

    println!(
        "\n=== Aggregate ({} txs, {} skipped) ===",
        results.len(),
        skipped
    );
    println!(
        "Total: Native={:.2}ms JIT={:.2}ms",
        total_native.as_secs_f64() * 1000.0,
        total_jit.as_secs_f64() * 1000.0
    );
    println!("Speedup JIT/Native: {aggregate:.2}x");
    println!(
        "JIT faster: {:.1}% ({}/{}), median {median:.2}x, min {min_s:.2}x, max {max_s:.2}x",
        jit_faster_count as f64 / results.len() as f64 * 100.0,
        jit_faster_count,
        results.len(),
    );
}

// ── Main ────────────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cache_dir = args
        .iter()
        .position(|a| a == "--cache-dir")
        .map(|i| PathBuf::from(args.get(i + 1).expect("--cache-dir requires <dir>")));
    let path = args
        .iter()
        .position(|a| a == "--path")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
        .expect("Usage: jsonl_bench --path <jsonl_file> [--cache-dir <dir>]");

    eprintln!("=== Loading JSONL ===");
    eprintln!("File: {path}");
    let (code_values, tx_records) = load_jsonl(path);
    eprintln!("Code values: {} unique bytecodes", code_values.len());
    eprintln!("Transactions: {}", tx_records.len());

    eprintln!("\n=== Compiling contracts ===");
    let start = Instant::now();
    let compiled = if let Some(cache_dir) = cache_dir.as_deref() {
        compile_all_contracts_with_cache(&code_values, OptimizationLevel::Aggressive, cache_dir)
    } else {
        compile_all_contracts(&code_values)
    };
    eprintln!("  Compilation time: {:.2}s", start.elapsed().as_secs_f64());

    eprintln!("\n=== Benchmark: Native vs JIT ===");
    let mut results = Vec::new();
    let mut skipped = 0usize;
    let mut gas_mismatches = 0usize;

    for tx_rec in &tx_records {
        // Skip deposit transactions (OP Stack specific, tx_type=0x7E)
        if tx_rec.tx.tx_type == 0x7E {
            eprintln!("  SKIP tx#{}: deposit transaction", tx_rec.tx_index);
            skipped += 1;
            continue;
        }

        let db_plain = build_cache_db(tx_rec, &code_values);
        let db_jit = build_cache_db(tx_rec, &code_values);
        let tx_env = build_tx_env(tx_rec);

        // Run plain (native interpreter)
        let (plain_gas, plain_time) = match execute_plain(db_plain, tx_env.clone()) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("  SKIP tx#{}: plain failed: {e}", tx_rec.tx_index);
                skipped += 1;
                continue;
            }
        };

        // Run JIT (revmc compiled)
        let (jit_gas, jit_time) = match execute_jit(db_jit, tx_env, &compiled.functions) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("  SKIP tx#{}: JIT failed: {e}", tx_rec.tx_index);
                skipped += 1;
                continue;
            }
        };

        // Check gas match
        if plain_gas != jit_gas {
            eprintln!(
                "  WARN tx#{}: gas mismatch! plain={plain_gas} jit={jit_gas}",
                tx_rec.tx_index
            );
            gas_mismatches += 1;
        }

        results.push(TxResult {
            tx_index: tx_rec.tx_index,
            gas: plain_gas,
            native_us: plain_time.as_secs_f64() * 1_000_000.0,
            jit_us: jit_time.as_secs_f64() * 1_000_000.0,
        });
    }

    print_results(&results, skipped);

    if gas_mismatches > 0 {
        eprintln!("\nWARNING: {gas_mismatches} gas mismatches detected!");
    }
}
