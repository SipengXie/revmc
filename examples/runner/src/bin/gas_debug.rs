//! Single-tx gas debug: compare plain vs JIT gas for one transaction.
//!
//! Usage: gas_debug --path <jsonl> --tx <tx_index>

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use op_revm::{DefaultOp, OpEvm, OpHaltReason, OpSpecId, OpTransactionError};
use op_revm::transaction::OpTransaction;
use revm::{
    bytecode::Bytecode,
    context::{CfgEnv, Context, TxEnv},
    context_interface::result::{EVMError, ResultAndState},
    database::{CacheDB, EmptyDB},
    handler::{EvmTr, FrameResult, Handler, ItemOrResult},
    interpreter::interpreter_types::Jumps,
    primitives::{
        hardfork::SpecId, keccak256, Address, Bytes, HashMap as RevmHashMap, StorageKey, StorageValue,
        TxKind, B256, U256,
    },
    state::AccountInfo,
};
use revmc::{EvmCompiler, EvmLlvmBackend, OptimizationLevel};
use revmc_builtins as _;
use revmc_context::{EvmCompilerFn, RawEvmCompilerFn};
use serde::Deserialize;

const OP_SPEC: OpSpecId = OpSpecId::ISTHMUS;
const ETH_SPEC: SpecId = OP_SPEC.into_eth_spec();

#[derive(Clone, Debug, PartialEq, Eq)]
enum TraceFrameInput {
    Call {
        scheme: revm::interpreter::interpreter_action::CallScheme,
        bytecode_address: Address,
        target_address: Address,
        caller: Address,
        call_value: U256,
        gas_limit: u64,
        is_static: bool,
        input_len: usize,
        input_repr: String,
        input_hash: B256,
        return_len: usize,
        return_offset: usize,
    },
    Create {
        gas_limit: u64,
        caller: Address,
        value: U256,
        init_code_len: usize,
    },
    ReturnFromFrame,
    ResumeParent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TraceEvent {
    compiled: bool,
    depth: usize,
    from: Address,
    pc: usize,
    gas_remaining: u64,
    stack_len: usize,
    stack_top: Option<String>,
    return_ir: Option<String>,
    return_gas_remaining: Option<u64>,
    input: TraceFrameInput,
}

// ── JSONL Serde (copied from jsonl_bench) ───────────────────────────────────

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

// ── Helpers (copied from jsonl_bench) ───────────────────────────────────────

fn parse_u256_hex(s: &str) -> U256 {
    let trimmed = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    if trimmed.is_empty() { U256::ZERO } else { U256::from_str_radix(trimmed, 16).unwrap_or_default() }
}

fn parse_b256_hex(s: &str) -> B256 {
    let trimmed = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    let padded = if trimmed.len() < 64 { format!("{:0>64}", trimmed) } else { trimmed.to_string() };
    B256::from_slice(&hex::decode(&padded).expect("invalid B256 hex"))
}

fn extract_bytecode(debug_str: &str) -> Bytecode {
    let hex_str = if debug_str.contains("bytecode:") {
        let start = debug_str.find("0x").unwrap();
        let end = debug_str[start..].find(|c: char| !c.is_ascii_hexdigit() && c != 'x').map(|i| start + i).unwrap_or(debug_str.len());
        &debug_str[start..end]
    } else if debug_str.starts_with("0x") {
        debug_str
    } else {
        panic!("unknown bytecode format")
    };
    Bytecode::new_raw(Bytes::from(hex::decode(hex_str.trim_start_matches("0x")).expect("hex")))
}

fn build_cache_db(tx: &TxRecordLine, code_values: &HashMap<B256, Bytecode>) -> CacheDB<EmptyDB> {
    let mut db = CacheDB::new(EmptyDB::new());
    let mut storage_by_addr: HashMap<String, RevmHashMap<StorageKey, StorageValue>> = HashMap::new();
    for sv in &tx.read.storage_values {
        storage_by_addr.entry(sv.addr.clone()).or_default().insert(parse_u256_hex(&sv.slot), parse_u256_hex(&sv.value));
    }
    for acc in &tx.read.account_values {
        let addr: Address = acc.addr.parse().expect("addr");
        let (nonce, balance, code_hash) = match (&acc.nonce, &acc.balance, &acc.code_hash) {
            (Some(n), Some(b), Some(ch)) => (*n, parse_u256_hex(b), parse_b256_hex(ch)),
            _ => continue,
        };
        let bytecode = code_values.get(&code_hash).cloned();
        db.insert_account_info(addr, AccountInfo { balance, nonce, code_hash, code: bytecode });
        if let Some(storage) = storage_by_addr.remove(&acc.addr) {
            db.replace_account_storage(addr, storage).unwrap();
        }
    }
    db
}

fn build_tx_env(tx: &TxRecordLine) -> OpTransaction<TxEnv> {
    let caller: Address = tx.tx.caller.parse().unwrap();
    let value = parse_u256_hex(&tx.tx.value);
    let data = Bytes::from(hex::decode(tx.tx.input.trim_start_matches("0x")).unwrap_or_default());
    let kind = match tx.tx.kind.as_str() {
        "call" => TxKind::Call(tx.tx.to.as_ref().unwrap().parse().unwrap()),
        "create" => TxKind::Create,
        other => panic!("unknown kind: {other}"),
    };
    let base = TxEnv {
        tx_type: tx.tx.tx_type, caller, gas_limit: tx.tx.gas_limit, gas_price: tx.tx.gas_price as u128,
        kind, value, data, nonce: tx.tx.nonce, chain_id: Some(tx.tx.chain_id),
        access_list: Default::default(), gas_priority_fee: None, blob_hashes: Vec::new(),
        max_fee_per_blob_gas: 0, authorization_list: Vec::new(),
    };
    let mut op_tx = OpTransaction::new(base);
    op_tx.enveloped_tx = Some(Bytes::new());
    op_tx
}

// ── JIT Handler ─────────────────────────────────────────────────────────────

type BenchEvm = OpEvm<op_revm::OpContext<CacheDB<EmptyDB>>, ()>;
type BenchError = EVMError<core::convert::Infallible, OpTransactionError>;

struct JitHandler {
    functions: Arc<HashMap<B256, RawEvmCompilerFn>>,
    trace_calls: bool,
    trace: Vec<TraceEvent>,
}

#[inline]
fn should_lookup_jit(
    frame_is_create: bool,
    bytecode_address: Option<Address>,
    bytecode_is_empty: bool,
) -> bool {
    !frame_is_create && bytecode_address.is_some() && !bytecode_is_empty
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
            let raw_fn = {
                let frame_stack = &mut evm.0.frame_stack;
                let frame = frame_stack.get();
                if should_lookup_jit(
                    frame.data.is_create(),
                    frame.interpreter.input.bytecode_address,
                    frame.interpreter.bytecode.is_empty(),
                ) {
                    let bytecode_hash = frame.interpreter.bytecode.get_or_calculate_hash();
                    self.functions.get(&bytecode_hash).copied()
                } else {
                    None
                }
            };

            let (compiled, call_or_result) = if let Some(raw_fn) = raw_fn {
                let (ctx, frame_stack) = (&mut evm.0.ctx, &mut evm.0.frame_stack);
                let frame = frame_stack.get();
                let f = EvmCompilerFn::new(raw_fn);
                let action = unsafe { f.call_with_interpreter(&mut frame.interpreter, ctx) };
                let item_or_result =
                    frame.process_next_action::<_, BenchError>(ctx, action).inspect(|i| {
                        if i.is_result() {
                            frame.set_finished(true);
                        }
                    })?;
                (true, item_or_result)
            } else {
                (false, evm.frame_run()?)
            };

            if self.trace_calls {
                if let ItemOrResult::Item(init) = &call_or_result {
                    let (ctx, frame_stack) = (&mut evm.0.ctx, &mut evm.0.frame_stack);
                    let frame = frame_stack.get();
                    let depth = init.depth;
                    let from = frame.interpreter.input.target_address;
                    let pc = frame.interpreter.bytecode.pc();
                    let gas_remaining = frame.interpreter.gas.remaining();
                    let stack_len = frame.interpreter.stack.len();
                    let stack_top = frame
                        .interpreter
                        .stack
                        .data()
                        .last()
                        .map(|w| hex::encode(w.to_be_bytes::<32>()));
                    let input = match &init.frame_input {
                        revm::interpreter::interpreter_action::FrameInput::Call(call) => {
                            let call = call.as_ref();
                            let input_bytes = call.input.bytes(ctx);
                            let input_head_len = input_bytes.len().min(16);
                            let input_repr = hex::encode(&input_bytes[..input_head_len]);
                            let input_hash = keccak256(&input_bytes);
                            Some(TraceFrameInput::Call {
                                scheme: call.scheme,
                                bytecode_address: call.bytecode_address,
                                target_address: call.target_address,
                                caller: call.caller,
                                call_value: call.call_value(),
                                gas_limit: call.gas_limit,
                                is_static: call.is_static,
                                input_len: call.input.len(),
                                input_repr,
                                input_hash,
                                return_len: call.return_memory_offset.len(),
                                return_offset: call.return_memory_offset.start,
                            })
                        }
                        revm::interpreter::interpreter_action::FrameInput::Create(create) => {
                            let create = create.as_ref();
                            Some(TraceFrameInput::Create {
                                gas_limit: create.gas_limit,
                                caller: create.caller,
                                value: create.value,
                                init_code_len: create.init_code.len(),
                            })
                        }
                        revm::interpreter::interpreter_action::FrameInput::Empty => None,
                    };
                    if let Some(input) = input {
                        self.trace.push(TraceEvent {
                            compiled,
                            depth,
                            from,
                            pc,
                            gas_remaining,
                            stack_len,
                            stack_top,
                            return_ir: None,
                            return_gas_remaining: None,
                            input,
                        });
                    }
                }
            }
            let result = match call_or_result {
                ItemOrResult::Item(init) => match evm.frame_init(init)? {
                    ItemOrResult::Item(_) => continue,
                    ItemOrResult::Result(result) => result,
                },
                ItemOrResult::Result(result) => result,
            };

            if self.trace_calls {
                let frame_stack = &mut evm.0.frame_stack;
                let depth = frame_stack.index().map(|i| i + 1).unwrap_or(0);
                if depth != 0 {
                    let frame = frame_stack.get();
                    let from = frame.interpreter.input.target_address;
                    let pc = frame.interpreter.bytecode.pc();
                    let gas_remaining = frame.interpreter.gas.remaining();
                    let stack_len = frame.interpreter.stack.len();
                    let stack_top = frame
                        .interpreter
                        .stack
                        .data()
                        .last()
                        .map(|w| hex::encode(w.to_be_bytes::<32>()));
                    let bytecode_hash = frame.interpreter.bytecode.get_or_calculate_hash();
                    let compiled = self.functions.contains_key(&bytecode_hash);
                    self.trace.push(TraceEvent {
                        compiled,
                        depth,
                        from,
                        pc,
                        gas_remaining,
                        stack_len,
                        stack_top,
                        return_ir: Some(format!("{:?}", result.instruction_result())),
                        return_gas_remaining: Some(result.gas().remaining()),
                        input: TraceFrameInput::ReturnFromFrame,
                    });
                }
            }

            let maybe_final = evm.frame_return_result(result)?;
            if let Some(result) = maybe_final {
                return Ok(result);
            }

            if self.trace_calls {
                let frame_stack = &mut evm.0.frame_stack;
                let depth = frame_stack.index().map(|i| i + 1).unwrap_or(0);
                if depth != 0 {
                    let frame = frame_stack.get();
                    let from = frame.interpreter.input.target_address;
                    let pc = frame.interpreter.bytecode.pc();
                    let gas_remaining = frame.interpreter.gas.remaining();
                    let stack_len = frame.interpreter.stack.len();
                    let stack_top = frame
                        .interpreter
                        .stack
                        .data()
                        .last()
                        .map(|w| hex::encode(w.to_be_bytes::<32>()));
                    let bytecode_hash = frame.interpreter.bytecode.get_or_calculate_hash();
                    let compiled = self.functions.contains_key(&bytecode_hash);
                    self.trace.push(TraceEvent {
                        compiled,
                        depth,
                        from,
                        pc,
                        gas_remaining,
                        stack_len,
                        stack_top,
                        return_ir: None,
                        return_gas_remaining: None,
                        input: TraceFrameInput::ResumeParent,
                    });
                }
            }
        }
    }
}

fn parse_opt_level(args: &[String]) -> OptimizationLevel {
    if args.iter().any(|a| a == "--o0") {
        OptimizationLevel::None
    } else if args.iter().any(|a| a == "--o1") {
        OptimizationLevel::Less
    } else if args.iter().any(|a| a == "--o2") {
        OptimizationLevel::Default
    } else {
        OptimizationLevel::Aggressive
    }
}

#[derive(Default)]
struct CacheStats {
    hits: usize,
    misses: usize,
}

struct CacheArtifacts {
    key: String,
    symbol: String,
    object: PathBuf,
    library: PathBuf,
}

struct CachedFunctions {
    functions: Arc<HashMap<B256, RawEvmCompilerFn>>,
    _libraries: Vec<libloading::Library>,
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
    CacheArtifacts { key, symbol, object, library }
}

fn load_cached_symbol(
    artifacts: &CacheArtifacts,
) -> Result<(RawEvmCompilerFn, libloading::Library), String> {
    let library = unsafe { libloading::Library::new(&artifacts.library) }
        .map_err(|e| format!("dlopen {} failed: {e}", artifacts.library.display()))?;
    let symbol = unsafe { library.get::<RawEvmCompilerFn>(artifacts.symbol.as_bytes()) }
        .map_err(|e| format!("dlsym {} failed in {}: {e}", artifacts.symbol, artifacts.library.display()))?;
    let function = *symbol;
    drop(symbol);
    Ok((function, library))
}

fn compile_contract_to_cache(
    bytecode: &Bytecode,
    opt: OptimizationLevel,
    artifacts: &CacheArtifacts,
    dump_ir_dir: Option<&Path>,
) -> Result<(), String> {
    if let Some(parent) = artifacts.object.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create cache dir {} failed: {e}", parent.display()))?;
    }

    let context = Box::leak(Box::new(revmc::llvm::inkwell::context::Context::create()));
    let backend = EvmLlvmBackend::new(context, true, opt)
        .map_err(|e| format!("AOT backend init failed ({}): {e}", artifacts.key))?;
    let mut compiler = EvmCompiler::new(backend);
    if let Some(dir) = dump_ir_dir {
        compiler.set_dump_to(Some(dir.to_path_buf()));
    }
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
    dump_ir_dir: Option<&Path>,
) -> Option<RawEvmCompilerFn> {
    let symbol = format!("c_{}", &hex::encode(hash)[..16]);
    let context = Box::leak(Box::new(revmc::llvm::inkwell::context::Context::create()));
    let backend = EvmLlvmBackend::new(context, false, opt).ok()?;
    let compiler: &'static mut EvmCompiler<EvmLlvmBackend<'static>> =
        Box::leak(Box::new(EvmCompiler::new(backend)));
    if let Some(dir) = dump_ir_dir {
        compiler.set_dump_to(Some(dir.to_path_buf()));
    }
    let func_id = compiler.translate(&symbol, bytecode.original_byte_slice(), ETH_SPEC).ok()?;
    unsafe { compiler.jit_function(func_id).ok().map(|f| f.into_inner()) }
}

fn load_or_compile_cached_contract(
    hash: B256,
    bytecode: &Bytecode,
    opt: OptimizationLevel,
    cache_dir: &Path,
    dump_ir_dir: Option<&Path>,
    stats: &mut CacheStats,
) -> Option<(RawEvmCompilerFn, libloading::Library)> {
    let artifacts = cache_artifacts(cache_dir, &hash, opt);
    let has_cache = artifacts.object.exists() && artifacts.library.exists();

    if has_cache {
        match load_cached_symbol(&artifacts) {
            Ok(loaded) => {
                stats.hits += 1;
                return Some(loaded);
            }
            Err(e) => {
                eprintln!(
                    "  Cache stale/unloadable for {}.. ({}), recompiling",
                    &hex::encode(hash)[..16],
                    e
                );
            }
        }
    }

    stats.misses += 1;
    if let Err(e) = compile_contract_to_cache(bytecode, opt, &artifacts, dump_ir_dir) {
        eprintln!("  WARN cache compile failed {}..: {e}", &hex::encode(hash)[..16]);
        return None;
    }

    match load_cached_symbol(&artifacts) {
        Ok(loaded) => Some(loaded),
        Err(e) => {
            eprintln!("  WARN cache load after compile failed {}..: {e}", &hex::encode(hash)[..16]);
            None
        }
    }
}

fn compile_contracts_with_cache(
    contracts: &[(B256, &Bytecode)],
    opt: OptimizationLevel,
    cache_dir: &Path,
    dump_ir_dir: Option<&Path>,
) -> CachedFunctions {
    std::fs::create_dir_all(cache_dir)
        .unwrap_or_else(|e| panic!("failed to create cache dir {}: {e}", cache_dir.display()));

    let mut functions = HashMap::new();
    let mut libraries = Vec::new();
    let mut stats = CacheStats::default();
    let mut jit_fallbacks = 0usize;

    for &(hash, bytecode) in contracts {
        if functions.contains_key(&hash) {
            continue;
        }
        if let Some((function, library)) =
            load_or_compile_cached_contract(hash, bytecode, opt, cache_dir, dump_ir_dir, &mut stats)
        {
            functions.insert(hash, function);
            libraries.push(library);
            continue;
        }

        if let Some(function) = compile_contract_jit_fallback(hash, bytecode, opt, dump_ir_dir) {
            jit_fallbacks += 1;
            functions.insert(hash, function);
            eprintln!(
                "  WARN using JIT fallback for {}.. (cache unavailable)",
                &hex::encode(hash)[..16]
            );
        }
    }

    eprintln!(
        "Cache stats: hits={} misses={} jit_fallbacks={}",
        stats.hits, stats.misses, jit_fallbacks
    );
    CachedFunctions { functions: Arc::new(functions), _libraries: libraries }
}

fn load_jsonl_all(path: &str) -> (HashMap<B256, Bytecode>, Vec<TxRecordLine>) {
    let file = File::open(path).expect("failed to open JSONL");
    let reader = BufReader::new(file);
    let mut lines = reader.lines();

    let first_line = lines.next().expect("empty file").expect("read error");
    let code_line: CodeValuesLine = serde_json::from_str(&first_line).expect("parse code_values");
    let mut code_values = HashMap::new();
    for (hash_str, bytecode_str) in &code_line.items {
        code_values.insert(parse_b256_hex(hash_str), extract_bytecode(bytecode_str));
    }

    let mut tx_records = Vec::new();
    for line in lines {
        let line = line.expect("read error");
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(rec) = serde_json::from_str::<TxRecordLine>(&line) {
            tx_records.push(rec);
        }
    }

    (code_values, tx_records)
}

fn compile_all_contracts(
    code_values: &HashMap<B256, Bytecode>,
    opt: OptimizationLevel,
) -> Arc<HashMap<B256, RawEvmCompilerFn>> {
    let mut contracts: Vec<(B256, &Bytecode)> = code_values
        .iter()
        .filter(|(_, bc)| !bc.is_empty())
        .map(|(h, bc)| (*h, bc))
        .collect();
    contracts.sort_by_key(|(_, bc)| std::cmp::Reverse(bc.original_byte_slice().len()));

    eprintln!(
        "Compiling {} non-empty contracts with ETH_SPEC={ETH_SPEC:?}, opt={opt:?}",
        contracts.len()
    );
    let n_threads = std::thread::available_parallelism()
        .map(|n| n.get().min(16))
        .unwrap_or(4);
    eprintln!("Compile threads: {n_threads}");

    // Round-robin assignment so large contracts are spread across threads.
    let mut assignments: Vec<Vec<(B256, &Bytecode)>> = vec![Vec::new(); n_threads];
    for (i, contract) in contracts.into_iter().enumerate() {
        assignments[i % n_threads].push(contract);
    }

    let thread_results: Vec<HashMap<B256, RawEvmCompilerFn>> = std::thread::scope(|s| {
        let handles: Vec<_> = assignments
            .into_iter()
            .enumerate()
            .map(|(tid, chunk)| {
                s.spawn(move || {
                    let context = Box::leak(Box::new(revmc::llvm::inkwell::context::Context::create()));
                    let backend = EvmLlvmBackend::new(context, false, opt).expect("LLVM backend");
                    let compiler: &'static mut EvmCompiler<EvmLlvmBackend<'static>> =
                        Box::leak(Box::new(EvmCompiler::new(backend)));

                    let mut pending = Vec::new();
                    for (hash, bc) in &chunk {
                        let name = format!("c_{}", &hex::encode(hash)[..16]);
                        match compiler.translate(&name, bc.original_byte_slice(), ETH_SPEC) {
                            Ok(func_id) => pending.push((*hash, func_id)),
                            Err(e) => eprintln!("  [T{tid}] WARN translate failed {name}: {e}"),
                        }
                    }

                    let mut functions = HashMap::new();
                    for (hash, func_id) in pending {
                        match unsafe { compiler.jit_function(func_id) } {
                            Ok(fn_ptr) => {
                                functions.insert(hash, fn_ptr.into_inner());
                            }
                            Err(e) => {
                                eprintln!("  [T{tid}] WARN JIT failed {}: {e}", &hex::encode(hash)[..16])
                            }
                        }
                    }
                    eprintln!("  [T{tid}] compiled {}", functions.len());
                    functions
                })
            })
            .collect();

        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut functions = HashMap::new();
    for thread_map in thread_results {
        functions.extend(thread_map);
    }
    eprintln!("Compiled functions: {}", functions.len());
    Arc::new(functions)
}

fn compile_all_contracts_with_cache(
    code_values: &HashMap<B256, Bytecode>,
    opt: OptimizationLevel,
    cache_dir: &Path,
) -> CachedFunctions {
    let mut contracts: Vec<(B256, &Bytecode)> = code_values
        .iter()
        .filter(|(_, bc)| !bc.is_empty())
        .map(|(h, bc)| (*h, bc))
        .collect();
    contracts.sort_by_key(|(_, bc)| std::cmp::Reverse(bc.original_byte_slice().len()));

    eprintln!(
        "Compiling/loading {} non-empty contracts with persistent cache dir={} ETH_SPEC={ETH_SPEC:?} opt={opt:?}",
        contracts.len(),
        cache_dir.display()
    );
    compile_contracts_with_cache(&contracts, opt, cache_dir, None)
}

fn run_full_correctness(path: &str, args: &[String], cache_dir: Option<&Path>) -> i32 {
    let (code_values, tx_records) = load_jsonl_all(path);
    eprintln!("Loaded code_values={} tx_records={}", code_values.len(), tx_records.len());

    let opt = parse_opt_level(args);
    let _cache_loaded = cache_dir.map(|dir| compile_all_contracts_with_cache(&code_values, opt, dir));
    let functions = if let Some(loaded) = _cache_loaded.as_ref() {
        loaded.functions.clone()
    } else {
        compile_all_contracts(&code_values, opt)
    };
    let empty_functions: Arc<HashMap<B256, RawEvmCompilerFn>> = Arc::new(HashMap::new());

    let mut matched = 0usize;
    let mut mismatched = 0usize;
    let mut plain_failed = 0usize;
    let mut jit_failed = 0usize;

    for (i, tx_rec) in tx_records.iter().enumerate() {
        if i % 25 == 0 {
            eprintln!("Progress: {}/{}", i, tx_records.len());
        }

        let tx_env = build_tx_env(tx_rec);
        let mut cfg: CfgEnv<OpSpecId> = CfgEnv::new_with_spec(OP_SPEC);
        cfg.chain_id = tx_env.base.chain_id.unwrap_or(8453);
        cfg.tx_chain_id_check = false;

        let plain = {
            let db_plain = build_cache_db(tx_rec, &code_values);
            let ctx = Context::op().with_db(db_plain).with_cfg(cfg.clone()).with_tx(tx_env.clone());
            let mut evm: OpEvm<_, ()> = OpEvm::new(ctx, ());
            let mut handler_plain = JitHandler {
                functions: empty_functions.clone(),
                trace_calls: false,
                trace: Vec::new(),
            };
            match handler_plain.run(&mut evm) {
                Ok(frame_result) => {
                    let state = evm.ctx().journaled_state.finalize();
                    Ok(ResultAndState::new(frame_result, state))
                }
                Err(e) => Err(format!("{e:?}")),
            }
        };

        let jit = {
            let db_jit = build_cache_db(tx_rec, &code_values);
            let ctx = Context::op().with_db(db_jit).with_cfg(cfg).with_tx(tx_env);
            let mut evm: OpEvm<_, ()> = OpEvm::new(ctx, ());
            let mut handler = JitHandler { functions: functions.clone(), trace_calls: false, trace: Vec::new() };
            match handler.run(&mut evm) {
                Ok(frame_result) => {
                    let state = evm.ctx().journaled_state.finalize();
                    Ok(ResultAndState::new(frame_result, state))
                }
                Err(e) => Err(format!("{e:?}")),
            }
        };

        let is_match = match (&plain, &jit) {
            (Ok(a), Ok(b)) => a == b,
            (Err(a), Err(b)) => a == b,
            _ => false,
        };

        if is_match {
            matched += 1;
            continue;
        }

        mismatched += 1;
        match (&plain, &jit) {
            (Ok(a), Ok(b)) => {
                eprintln!(
                    "MISMATCH tx#{}: result_eq={} state_eq={} gas_plain={} gas_jit={} state_plain={} state_jit={}",
                    tx_rec.tx_index,
                    a.result == b.result,
                    a.state == b.state,
                    a.result.gas_used(),
                    b.result.gas_used(),
                    a.state.len(),
                    b.state.len(),
                );
            }
            (Err(a), Err(b)) => {
                plain_failed += 1;
                jit_failed += 1;
                eprintln!("MISMATCH tx#{}: plain_err={a} jit_err={b}", tx_rec.tx_index);
            }
            (Err(a), Ok(_)) => {
                plain_failed += 1;
                eprintln!("MISMATCH tx#{}: plain_err={a} jit_ok", tx_rec.tx_index);
            }
            (Ok(_), Err(b)) => {
                jit_failed += 1;
                eprintln!("MISMATCH tx#{}: plain_ok jit_err={b}", tx_rec.tx_index);
            }
        }
    }

    eprintln!("\n=== Full Correctness Summary ===");
    eprintln!("Total tx: {}", tx_records.len());
    eprintln!("MATCH:    {matched}");
    eprintln!("MISMATCH: {mismatched}");
    eprintln!("plain_err: {plain_failed}, jit_err: {jit_failed}");
    if mismatched == 0 {
        eprintln!("ALL MATCH (result + state + gas)");
        0
    } else {
        1
    }
}

// ── Main ────────────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.iter().position(|a| a == "--path").and_then(|i| args.get(i + 1)).map(|s| s.as_str()).expect("--path required");
    let cache_dir = args
        .iter()
        .position(|a| a == "--cache-dir")
        .map(|i| PathBuf::from(args.get(i + 1).expect("--cache-dir requires <dir>")));
    let target_tx: Option<u64> = args
        .iter()
        .position(|a| a == "--tx")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok());
    let trace_calls = args.iter().any(|a| a == "--trace-calls") || std::env::var("REVM_C_TRACE_CALLS").is_ok();

    if target_tx.is_none() {
        if trace_calls {
            eprintln!("WARN: --trace-calls is ignored in full mode");
        }
        let exit_code = run_full_correctness(path, &args, cache_dir.as_deref());
        if exit_code != 0 {
            std::process::exit(exit_code);
        }
        return;
    }
    let target_tx = target_tx.expect("--tx parsing should have succeeded");

    eprintln!("Loading JSONL: {path}");
    let file = File::open(path).unwrap();
    let reader = BufReader::new(file);
    let mut lines = reader.lines();

    let first_line = lines.next().unwrap().unwrap();
    let code_line: CodeValuesLine = serde_json::from_str(&first_line).unwrap();
    let mut code_values = HashMap::new();
    for (hash_str, bytecode_str) in &code_line.items {
        code_values.insert(parse_b256_hex(hash_str), extract_bytecode(bytecode_str));
    }

    let mut tx_rec = None;
    for line in lines {
        let line = line.unwrap();
        if line.trim().is_empty() { continue; }
        if let Ok(rec) = serde_json::from_str::<TxRecordLine>(&line) {
            if rec.tx_index == target_tx {
                tx_rec = Some(rec);
                break;
            }
        }
    }
    let tx_rec = tx_rec.unwrap_or_else(|| panic!("tx#{target_tx} not found"));

    eprintln!("tx#{}: to={:?} gas_limit={} caller={}", tx_rec.tx_index, tx_rec.tx.to, tx_rec.tx.gas_limit, tx_rec.tx.caller);
    eprintln!("OP_SPEC={OP_SPEC:?} ETH_SPEC={ETH_SPEC:?}");

    // Find which bytecodes this tx touches
    let db = build_cache_db(&tx_rec, &code_values);
    let mut touched_codes: Vec<(B256, usize)> = Vec::new();
    for acc in &tx_rec.read.account_values {
        if let Some(ch) = &acc.code_hash {
            let hash = parse_b256_hex(ch);
            if let Some(bc) = code_values.get(&hash) {
                if !bc.is_empty() {
                    touched_codes.push((hash, bc.original_byte_slice().len()));
                }
            }
        }
    }
    touched_codes.sort_by_key(|(_, len)| std::cmp::Reverse(*len));
    eprintln!("Touched bytecodes: {} (non-empty)", touched_codes.len());
    for (hash, len) in &touched_codes {
        eprintln!("  {}: {} bytes", &hex::encode(hash)[..16], len);
    }

    // Compile only the bytecodes this tx touches
    eprintln!("\nCompiling {} contracts with ETH_SPEC={ETH_SPEC:?}...", touched_codes.len());
    let opt = parse_opt_level(&args);
    eprintln!("  Optimization level: {opt:?}");
    let separate_modules = args.iter().any(|a| a == "--separate-modules");
    let dump_ir_dir = if let Some(pos) = args.iter().position(|a| a == "--dump-ir") {
        let dir = args.get(pos + 1).map(|s| std::path::PathBuf::from(s))
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp/revmc-ir"));
        eprintln!("  Dumping IR to: {}", dir.display());
        Some(dir)
    } else {
        None
    };

    // --only <prefix> filters which contracts to JIT-compile
    let only_prefixes: Vec<String> = {
        let mut v = Vec::new();
        let mut i = 0;
        while i < args.len() {
            if args[i] == "--only" {
                if let Some(prefix) = args.get(i + 1) {
                    v.push(prefix.to_lowercase());
                    i += 2;
                    continue;
                }
            }
            i += 1;
        }
        v
    };
    if !only_prefixes.is_empty() {
        eprintln!("  Only compiling contracts matching: {:?}", only_prefixes);
    }
    let should_compile = |hash: &B256| -> bool {
        if only_prefixes.is_empty() {
            return true;
        }
        let hex = hex::encode(hash);
        only_prefixes.iter().any(|p| hex.starts_with(p.as_str()))
    };

    let mut _cache_loaded: Option<CachedFunctions> = None;
    let functions: Arc<HashMap<B256, RawEvmCompilerFn>> = if let Some(cache_dir) = cache_dir.as_deref() {
        eprintln!("  Mode: CACHE AOT (per-contract shared library)");
        if separate_modules {
            eprintln!("  NOTE: --separate-modules is ignored when --cache-dir is enabled");
        }

        let mut selected_contracts = Vec::new();
        for (hash, _) in &touched_codes {
            if !should_compile(hash) {
                continue;
            }
            if let Some(bc) = code_values.get(hash) {
                selected_contracts.push((*hash, bc));
            }
        }

        let loaded =
            compile_contracts_with_cache(&selected_contracts, opt, cache_dir, dump_ir_dir.as_deref());
        eprintln!("Compiled/loaded: {}/{}", loaded.functions.len(), selected_contracts.len());
        let functions = loaded.functions.clone();
        _cache_loaded = Some(loaded);
        functions
    } else {
        if separate_modules {
            eprintln!("  Mode: SEPARATE modules (one LLVM module per contract)");
        } else {
            eprintln!("  Mode: SHARED module (all contracts in one LLVM module)");
        }

        let mut functions = HashMap::new();
        if separate_modules {
            // Each contract gets its own LLVM Context + Module + Compiler
            for (hash, _) in &touched_codes {
                if !should_compile(hash) {
                    continue;
                }
                let bc = code_values.get(hash).unwrap();
                let name = format!("c_{}", &hex::encode(hash)[..16]);
                let context = Box::leak(Box::new(revmc::llvm::inkwell::context::Context::create()));
                let backend = EvmLlvmBackend::new(context, false, opt).unwrap();
                let compiler: &'static mut EvmCompiler<EvmLlvmBackend<'static>> =
                    Box::leak(Box::new(EvmCompiler::new(backend)));
                if let Some(ref dir) = dump_ir_dir {
                    compiler.set_dump_to(Some(dir.clone()));
                }
                match compiler.translate(&name, bc.original_byte_slice(), ETH_SPEC) {
                    Ok(func_id) => match unsafe { compiler.jit_function(func_id) } {
                        Ok(fn_ptr) => {
                            functions.insert(*hash, fn_ptr.into_inner());
                        }
                        Err(e) => eprintln!("  JIT failed {name}: {e}"),
                    },
                    Err(e) => eprintln!("  Translate failed {name}: {e}"),
                }
            }
        } else {
            // All contracts in one shared LLVM module
            let context = Box::leak(Box::new(revmc::llvm::inkwell::context::Context::create()));
            let backend = EvmLlvmBackend::new(context, false, opt).unwrap();
            let compiler: &'static mut EvmCompiler<EvmLlvmBackend<'static>> =
                Box::leak(Box::new(EvmCompiler::new(backend)));
            if let Some(ref dir) = dump_ir_dir {
                compiler.set_dump_to(Some(dir.clone()));
            }

            // Phase 1: translate ALL contracts into the LLVM module
            let mut pending = Vec::new();
            for (hash, _) in &touched_codes {
                if !should_compile(hash) {
                    continue;
                }
                let bc = code_values.get(hash).unwrap();
                let name = format!("c_{}", &hex::encode(hash)[..16]);
                match compiler.translate(&name, bc.original_byte_slice(), ETH_SPEC) {
                    Ok(func_id) => pending.push((*hash, func_id)),
                    Err(e) => eprintln!("  Translate failed {}: {e}", &name),
                }
            }
            // Phase 2: finalize module and extract function pointers
            for (hash, func_id) in pending {
                match unsafe { compiler.jit_function(func_id) } {
                    Ok(fn_ptr) => {
                        functions.insert(hash, fn_ptr.into_inner());
                    }
                    Err(e) => eprintln!("  JIT failed {}: {e}", &hex::encode(&hash.as_slice()[..8])),
                }
            }
        }
        eprintln!("Compiled: {}/{}", functions.len(), touched_codes.len());
        Arc::new(functions)
    };
    if let Some(entry_point) = tx_rec.tx.to.as_ref().and_then(|s| s.parse::<Address>().ok()) {
        let entry_hash = tx_rec
            .read
            .account_values
            .iter()
            .find(|a| a.addr.parse::<Address>().ok() == Some(entry_point))
            .and_then(|a| a.code_hash.as_ref())
            .map(|h| parse_b256_hex(h));
        if let Some(entry_hash) = entry_hash {
            eprintln!(
                "EntryPoint code_hash={}.. compiled={}",
                &hex::encode(entry_hash)[..16],
                functions.contains_key(&entry_hash)
            );
        } else {
            eprintln!("EntryPoint code_hash: <not found in account_values>");
        }
    }
    for hash in functions.keys() {
        let mut addrs = Vec::new();
        for acc in &tx_rec.read.account_values {
            if acc.code_hash.as_ref().map(|h| parse_b256_hex(h)) == Some(*hash) {
                addrs.push(acc.addr.clone());
            }
        }
        eprintln!("Compiled hash={}.. addrs={addrs:?}", &hex::encode(hash)[..16]);
    }

    // Run plain
    let tx_env = build_tx_env(&tx_rec);
    let db_plain = build_cache_db(&tx_rec, &code_values);
    let mut cfg: CfgEnv<OpSpecId> = CfgEnv::new_with_spec(OP_SPEC);
    cfg.chain_id = tx_env.base.chain_id.unwrap_or(8453);
    cfg.tx_chain_id_check = false;

    let ctx = Context::op().with_db(db_plain).with_cfg(cfg.clone()).with_tx(tx_env.clone());
    let mut evm: OpEvm<_, ()> = OpEvm::new(ctx, ());
    let mut handler_plain = JitHandler { functions: Arc::new(HashMap::new()), trace_calls, trace: Vec::new() };
    let plain_result = handler_plain.run(&mut evm);
    let (plain_gas, plain_status) = match &plain_result {
        Ok(r) => (r.gas_used(), format!("{:?}", r)),
        Err(e) => (0, format!("ERR: {e:?}")),
    };

    // Run JIT
    let db_jit = build_cache_db(&tx_rec, &code_values);
    let ctx = Context::op().with_db(db_jit).with_cfg(cfg).with_tx(tx_env.clone());
    let mut evm: OpEvm<_, ()> = OpEvm::new(ctx, ());
    let mut handler = JitHandler { functions, trace_calls, trace: Vec::new() };
    let jit_result = handler.run(&mut evm);
    let (jit_gas, jit_status) = match &jit_result {
        Ok(r) => (r.gas_used(), format!("{:?}", r)),
        Err(e) => (0, format!("ERR: {e:?}")),
    };

    if trace_calls {
        {
            eprintln!("\n=== Trace (All calls/creates) ===");
            eprintln!("Plain events: {}", handler_plain.trace.len());
            eprintln!("JIT events:   {}", handler.trace.len());
            let n = handler_plain.trace.len().min(handler.trace.len());
            for i in 0..n {
                let a = &handler_plain.trace[i];
                let b = &handler.trace[i];
                let same_site =
                    a.depth == b.depth && a.from == b.from && a.pc == b.pc && a.input == b.input;
                if !same_site {
                    eprintln!("First site mismatch at idx={i}");
                    eprintln!("  plain={a:?}");
                    eprintln!("  jit  ={b:?}");
                    break;
                }
                if a.gas_remaining != b.gas_remaining {
                    eprintln!(
                        "First caller gas_remaining mismatch at idx={i}: plain={} jit={}",
                        a.gas_remaining, b.gas_remaining
                    );
                    eprintln!("  site={:?}", a.input);
                    break;
                }
            }

            if let Ok(n_dump) = std::env::var("REVM_C_DUMP_ALL_EVENTS") {
                let n_dump = n_dump.parse::<usize>().unwrap_or(200);
                eprintln!("\n--- Plain all events (first {n_dump}) ---");
                for (i, e) in handler_plain.trace.iter().take(n_dump).enumerate() {
                    eprintln!(
                        "  plain_all[{i}] depth={} from={} pc={} gas_remaining={} stack_len={} stack_top={:?} return_ir={:?} return_gas_remaining={:?} input={:?}",
                        e.depth,
                        e.from,
                        e.pc,
                        e.gas_remaining,
                        e.stack_len,
                        e.stack_top,
                        e.return_ir,
                        e.return_gas_remaining,
                        e.input
                    );
                }
                eprintln!("\n--- JIT all events (first {n_dump}) ---");
                for (i, e) in handler.trace.iter().take(n_dump).enumerate() {
                    eprintln!(
                        "  jit_all[{i}] depth={} compiled={} from={} pc={} gas_remaining={} stack_len={} stack_top={:?} return_ir={:?} return_gas_remaining={:?} input={:?}",
                        e.depth,
                        e.compiled,
                        e.from,
                        e.pc,
                        e.gas_remaining,
                        e.stack_len,
                        e.stack_top,
                        e.return_ir,
                        e.return_gas_remaining,
                        e.input
                    );
                }
            }
        }

        let entry_point: Option<Address> = tx_rec.tx.to.as_ref().and_then(|s| s.parse().ok());
        if let Some(entry_point) = entry_point {
            let plain_calls: Vec<_> = handler_plain
                .trace
                .iter()
                .filter(|e| e.from == entry_point)
                .cloned()
                .collect();
            let jit_calls: Vec<_> = handler
                .trace
                .iter()
                .filter(|e| e.from == entry_point)
                .cloned()
                .collect();

            eprintln!("\n=== Trace (EntryPoint only) ===");
            eprintln!("Plain events: {}", plain_calls.len());
            eprintln!("JIT events:   {}", jit_calls.len());
            for (i, e) in plain_calls.iter().enumerate() {
                eprintln!(
                    "  plain[{i}] pc={} gas_remaining={} stack_len={} stack_top={:?} return_ir={:?} return_gas_remaining={:?} input={:?}",
                    e.pc,
                    e.gas_remaining,
                    e.stack_len,
                    e.stack_top,
                    e.return_ir,
                    e.return_gas_remaining,
                    e.input
                );
            }
            for (i, e) in jit_calls.iter().enumerate() {
                eprintln!(
                    "  jit[{i}] compiled={} pc={} gas_remaining={} stack_len={} stack_top={:?} return_ir={:?} return_gas_remaining={:?} input={:?}",
                    e.compiled,
                    e.pc,
                    e.gas_remaining,
                    e.stack_len,
                    e.stack_top,
                    e.return_ir,
                    e.return_gas_remaining,
                    e.input
                );
            }
            let n = plain_calls.len().min(jit_calls.len());
            for i in 0..n {
                let a = &plain_calls[i];
                let b = &jit_calls[i];
                // Compare non-gas identity fields first.
                let same_site = a.depth == b.depth && a.from == b.from && a.pc == b.pc && a.input == b.input;
                if !same_site {
                    eprintln!("First site mismatch at idx={i}");
                    eprintln!("  plain={a:?}");
                    eprintln!("  jit  ={b:?}");
                    break;
                }
                if a.gas_remaining != b.gas_remaining {
                    eprintln!("First caller gas_remaining mismatch at idx={i}: plain={} jit={}", a.gas_remaining, b.gas_remaining);
                    eprintln!("  site={:?}", a.input);
                    break;
                }
            }
        }
    }

    eprintln!("\n=== Results ===");
    eprintln!("Plain: gas={plain_gas} status={plain_status}");
    eprintln!("JIT:   gas={jit_gas} status={jit_status}");
    let delta = plain_gas as i64 - jit_gas as i64;
    if delta != 0 {
        eprintln!("MISMATCH: delta={delta} (plain-jit)");
    } else {
        eprintln!("MATCH: gas identical!");
    }
}
