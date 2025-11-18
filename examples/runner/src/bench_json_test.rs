use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use k256::ecdsa::SigningKey;
use revm::{
    db::{CacheDB, EmptyDB},
    handler::register::EvmHandler,
    primitives::{
        keccak256, AccountInfo, Address, BlobExcessGasAndPrice, BlockEnv, Bytecode, Bytes, CfgEnv,
        Env, HashMap as RevmHashMap, SpecId, TransactTo, TxEnv, B256, U256,
    },
    Evm,
};
use revmc::{EvmCompiler, EvmLlvmBackend, OptimizationLevel};
use revmc_context::{EvmCompilerFn, RawEvmCompilerFn};
use serde::Deserialize;

const FIXTURE_RELATIVE_PATH: &str = "data/uniswap-t100-c20.json";

pub fn bench_first_uniswap_tx(c: &mut Criterion) {
    let fixture_plain = Fixture::load().expect("failed to load JSON fixture");
    let fixture_jit = Fixture::load().expect("failed to load JSON fixture");

    let mut group = c.benchmark_group("uniswap_first_transaction");
    group.bench_function("plain_execution", move |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let mut evm = fixture_plain.prepare_plain_evm();
                let start = Instant::now();
                let result = evm.transact().expect("transaction execution failed");
                total += start.elapsed();
                black_box(result);
            }
            total
        });
    });
    group.bench_function("jit_optimized", move |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let mut evm = fixture_jit.prepare_evm();
                let start = Instant::now();
                let result = evm.transact().expect("transaction execution failed");
                total += start.elapsed();
                black_box(result);
            }
            total
        });
    });
    group.finish();
}

criterion_group!(benches, bench_first_uniswap_tx);
criterion_main!(benches);

struct Fixture {
    env: Env,
    accounts: Vec<PreparedAccount>,
    compiled: CompiledContracts,
}

#[derive(Clone)]
struct PreparedAccount {
    address: Address,
    info: AccountInfo,
    storage: RevmHashMap<U256, U256>,
}

impl Fixture {
    fn load() -> Result<Self, String> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join(FIXTURE_RELATIVE_PATH);
        Self::from_path(&path)
    }

    fn from_path(path: &Path) -> Result<Self, String> {
        let json = fs::read_to_string(path).map_err(|err| err.to_string())?;
        let test_file: RawTestFile = serde_json::from_str(&json).map_err(|err| err.to_string())?;
        let mut cases = test_file.cases.into_values();
        let raw_case =
            cases.next().ok_or_else(|| "fixture does not contain any test cases".to_owned())?;
        let first_tx = raw_case
            .transaction
            .into_iter()
            .next()
            .ok_or_else(|| "fixture does not contain any transactions".to_owned())?;

        let mut env = build_env(&raw_case.env)?;
        apply_transaction(&mut env, &first_tx)?;
        let accounts = parse_accounts(raw_case.pre)?;
        let compiled = compile_contracts(&accounts)?;

        Ok(Self { env, accounts, compiled })
    }

    fn prepare_evm(&self) -> Evm<'static, BenchExternalContext, CacheDB<EmptyDB>> {
        let mut evm = build_optimized_evm(self.populate_db(), self.compiled.functions.clone());
        *evm.context.evm.env = self.env.clone();
        evm
    }

    fn prepare_plain_evm(&self) -> Evm<'static, (), CacheDB<EmptyDB>> {
        let mut evm = build_plain_evm(self.populate_db());
        *evm.context.evm.env = self.env.clone();
        evm
    }

    fn populate_db(&self) -> CacheDB<EmptyDB> {
        let mut db = CacheDB::new(EmptyDB::new());
        for account in &self.accounts {
            db.insert_account_info(account.address, account.info.clone());
            if !account.storage.is_empty() {
                db.replace_account_storage(account.address, account.storage.clone())
                    .expect("failed to populate account storage");
            }
        }
        db
    }
}

#[derive(Debug, Deserialize)]
struct RawTestFile {
    #[serde(flatten)]
    cases: BTreeMap<String, RawCase>,
}

#[derive(Debug, Deserialize)]
struct RawCase {
    env: RawEnv,
    pre: BTreeMap<String, RawAccount>,
    transaction: Vec<RawTransaction>,
}

#[derive(Debug, Deserialize)]
struct RawEnv {
    #[serde(rename = "currentBaseFee")]
    current_base_fee: Option<String>,
    #[serde(rename = "currentCoinbase")]
    current_coinbase: Option<String>,
    #[serde(rename = "currentDifficulty")]
    current_difficulty: Option<String>,
    #[serde(rename = "currentExcessBlobGas")]
    current_excess_blob_gas: Option<String>,
    #[serde(rename = "currentGasLimit")]
    current_gas_limit: Option<String>,
    #[serde(rename = "currentNumber")]
    current_number: Option<String>,
    #[serde(rename = "currentRandom")]
    current_random: Option<String>,
    #[serde(rename = "currentTimestamp")]
    current_timestamp: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawAccount {
    balance: String,
    code: String,
    nonce: String,
    #[serde(default)]
    storage: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct RawTransaction {
    data: String,
    #[serde(rename = "gasLimit")]
    gas_limit: String,
    #[serde(default, rename = "gasPrice")]
    gas_price: Option<String>,
    #[serde(default, rename = "maxFeePerGas")]
    max_fee_per_gas: Option<String>,
    #[serde(default, rename = "maxPriorityFeePerGas")]
    max_priority_fee_per_gas: Option<String>,
    #[serde(default)]
    nonce: Option<String>,
    #[serde(rename = "secretKey")]
    secret_key: String,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    value: Option<String>,
    #[serde(default, rename = "blobVersionedHashes")]
    blob_hashes: Vec<String>,
    #[serde(default, rename = "maxFeePerBlobGas")]
    max_fee_per_blob_gas: Option<String>,
}

fn build_env(raw: &RawEnv) -> Result<Env, String> {
    let mut env = Env { cfg: CfgEnv::default(), block: BlockEnv::default(), tx: TxEnv::default() };

    env.block.number = parse_u256(
        raw.current_number.as_deref().ok_or_else(|| "missing env.currentNumber".to_owned())?,
    )?;
    env.block.timestamp = parse_u256(
        raw.current_timestamp
            .as_deref()
            .ok_or_else(|| "missing env.currentTimestamp".to_owned())?,
    )?;
    env.block.gas_limit = parse_u256(
        raw.current_gas_limit.as_deref().ok_or_else(|| "missing env.currentGasLimit".to_owned())?,
    )?;
    env.block.basefee = parse_u256(
        raw.current_base_fee.as_deref().ok_or_else(|| "missing env.currentBaseFee".to_owned())?,
    )?;
    env.block.coinbase = parse_address(
        raw.current_coinbase.as_deref().ok_or_else(|| "missing env.currentCoinbase".to_owned())?,
    )?;
    if let Some(difficulty) = &raw.current_difficulty {
        env.block.difficulty = parse_u256(difficulty)?;
    }
    env.block.prevrandao = match raw.current_random.as_deref() {
        Some(value) => Some(parse_b256(value)?),
        None => None,
    };
    match raw.current_excess_blob_gas.as_deref() {
        Some(value) => {
            let excess = parse_u64(value)?;
            env.block.blob_excess_gas_and_price.replace(BlobExcessGasAndPrice::new(excess, false));
        }
        None => env.block.blob_excess_gas_and_price = None,
    }

    Ok(env)
}

fn apply_transaction(env: &mut Env, tx: &RawTransaction) -> Result<(), String> {
    env.tx = TxEnv::default();
    env.tx.caller = derive_caller_address(&tx.secret_key)?;
    env.tx.gas_limit = parse_u64(&tx.gas_limit)?;

    let gas_price_source =
        tx.gas_price.as_deref().or(tx.max_fee_per_gas.as_deref()).unwrap_or("0x0");
    env.tx.gas_price = parse_u256(gas_price_source)?;
    env.tx.gas_priority_fee = match tx.max_priority_fee_per_gas.as_deref() {
        Some(value) => Some(parse_u256(value)?),
        None => None,
    };

    env.tx.value = parse_u256(tx.value.as_deref().unwrap_or("0x0"))?;
    env.tx.data = parse_bytes(&tx.data)?;
    env.tx.nonce = Some(parse_u64(tx.nonce.as_deref().unwrap_or("0x0"))?);
    env.tx.chain_id = Some(env.cfg.chain_id);
    env.tx.transact_to = match tx.to.as_deref() {
        Some(value) if value.trim().is_empty() || value.trim() == "0x" => TransactTo::Create,
        Some(value) => TransactTo::Call(parse_address(value)?),
        None => TransactTo::Create,
    };

    env.tx.blob_hashes =
        tx.blob_hashes.iter().map(|hash| parse_b256(hash)).collect::<Result<Vec<_>, _>>()?;
    env.tx.max_fee_per_blob_gas = match tx.max_fee_per_blob_gas.as_deref() {
        Some(value) => Some(parse_u256(value)?),
        None => None,
    };

    Ok(())
}

fn parse_accounts(pre: BTreeMap<String, RawAccount>) -> Result<Vec<PreparedAccount>, String> {
    let mut accounts = Vec::with_capacity(pre.len());
    for (address_hex, account) in pre {
        accounts.push(parse_account(&address_hex, account)?);
    }
    Ok(accounts)
}

fn parse_account(address_hex: &str, account: RawAccount) -> Result<PreparedAccount, String> {
    let address = parse_address(address_hex)?;
    let balance = parse_u256(&account.balance)?;
    let nonce = parse_u64(&account.nonce)?;

    let bytecode_bytes = parse_hex_bytes(&account.code)?;
    let bytecode = Bytecode::new_raw(Bytes::from(bytecode_bytes));
    let code_hash = bytecode.hash_slow();

    let storage = parse_storage(account.storage)?;

    let info = AccountInfo { balance, nonce, code_hash, code: Some(bytecode) };

    Ok(PreparedAccount { address, info, storage })
}

fn parse_storage(storage: BTreeMap<String, String>) -> Result<RevmHashMap<U256, U256>, String> {
    let mut entries = RevmHashMap::with_capacity(storage.len());
    for (slot_hex, value_hex) in storage {
        let slot = parse_u256(&slot_hex)?;
        let value = parse_u256(&value_hex)?;
        entries.insert(slot, value);
    }
    Ok(entries)
}

struct CompiledContracts {
    functions: Arc<HashMap<B256, RawEvmCompilerFn>>,
    #[allow(dead_code)]
    _compiler: &'static mut EvmCompiler<EvmLlvmBackend<'static>>,
    #[allow(dead_code)]
    _context: &'static revmc::llvm::inkwell::context::Context,
}

fn compile_contracts(accounts: &[PreparedAccount]) -> Result<CompiledContracts, String> {
    let context = Box::leak(Box::new(revmc::llvm::inkwell::context::Context::create()));
    let backend = EvmLlvmBackend::new(context, false, OptimizationLevel::Aggressive)
        .map_err(|err| err.to_string())?;
    let compiler: &'static mut EvmCompiler<EvmLlvmBackend<'static>> =
        Box::leak(Box::new(EvmCompiler::new(backend)));

    let mut seen = HashSet::new();
    let mut pending = Vec::new();
    for account in accounts {
        let Some(code) = account.info.code.as_ref() else { continue };
        if code.is_empty() {
            continue;
        }
        let hash = account.info.code_hash;
        if !seen.insert(hash) {
            continue;
        }

        let name = format!("contract_{}", hex::encode(hash.as_slice()));
        let func_id = compiler
            .translate(&name, code.original_byte_slice(), SpecId::CANCUN)
            .map_err(|err| err.to_string())?;
        pending.push((hash, func_id));
    }

    let mut functions = HashMap::with_capacity(pending.len());
    for (hash, func_id) in pending {
        let fn_ptr = unsafe { compiler.jit_function(func_id).map_err(|err| err.to_string())? };
        functions.insert(hash, fn_ptr.into_inner());
    }

    Ok(CompiledContracts { functions: Arc::new(functions), _compiler: compiler, _context: context })
}

fn derive_caller_address(secret_hex: &str) -> Result<Address, String> {
    let raw = parse_fixed_bytes(secret_hex, 32)?;
    let key_bytes: [u8; 32] =
        raw.try_into().map_err(|_| "secret key must be 32 bytes".to_owned())?;
    let signing_key = SigningKey::from_bytes(&key_bytes.into())
        .map_err(|err| format!("invalid secret key: {err}"))?;
    let verifying_key = signing_key.verifying_key();
    let encoded = verifying_key.to_encoded_point(false);
    let public_key = encoded.as_bytes();
    if public_key.len() != 65 {
        return Err("unexpected public key length".to_owned());
    }
    let hash = keccak256(&public_key[1..]);
    Ok(Address::from_slice(&hash[12..]))
}

fn parse_bytes(value: &str) -> Result<Bytes, String> {
    Ok(Bytes::from(parse_hex_bytes(value)?))
}

fn parse_address(value: &str) -> Result<Address, String> {
    let bytes = parse_fixed_bytes(value, 20)?;
    Ok(Address::from_slice(&bytes))
}

fn parse_b256(value: &str) -> Result<B256, String> {
    let bytes = parse_fixed_bytes(value, 32)?;
    Ok(B256::from_slice(&bytes))
}

fn parse_u256(value: &str) -> Result<U256, String> {
    let trimmed = strip_0x(value.trim());
    if trimmed.is_empty() {
        Ok(U256::ZERO)
    } else {
        U256::from_str_radix(trimmed, 16).map_err(|err| err.to_string())
    }
}

fn parse_u64(value: &str) -> Result<u64, String> {
    let trimmed = strip_0x(value.trim());
    if trimmed.is_empty() {
        Ok(0)
    } else {
        u64::from_str_radix(trimmed, 16).map_err(|err| err.to_string())
    }
}

fn parse_hex_bytes(value: &str) -> Result<Vec<u8>, String> {
    let trimmed = strip_0x(value.trim());
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let even_length =
        if trimmed.len() % 2 == 0 { trimmed.to_owned() } else { format!("0{trimmed}") };
    hex::decode(even_length).map_err(|err| err.to_string())
}

fn parse_fixed_bytes(value: &str, expected_len: usize) -> Result<Vec<u8>, String> {
    let mut bytes = parse_hex_bytes(value)?;
    if bytes.len() > expected_len {
        return Err(format!("value {value} exceeds expected length of {expected_len} bytes"));
    }
    if bytes.len() < expected_len {
        let mut padded = vec![0u8; expected_len - bytes.len()];
        padded.extend_from_slice(&bytes);
        bytes = padded;
    }
    Ok(bytes)
}

fn strip_0x(value: &str) -> &str {
    value.strip_prefix("0x").or_else(|| value.strip_prefix("0X")).unwrap_or(value)
}

struct BenchExternalContext {
    functions: Arc<HashMap<B256, RawEvmCompilerFn>>,
}

impl BenchExternalContext {
    fn new(functions: Arc<HashMap<B256, RawEvmCompilerFn>>) -> Self {
        Self { functions }
    }

    fn get_function(&self, hash: B256) -> Option<EvmCompilerFn> {
        self.functions.get(&hash).copied().map(EvmCompilerFn::new)
    }
}

fn build_optimized_evm<'a, DB: revm::Database + 'static>(
    db: DB,
    functions: Arc<HashMap<B256, RawEvmCompilerFn>>,
) -> Evm<'a, BenchExternalContext, DB> {
    revm::Evm::builder()
        .with_db(db)
        .with_external_context(BenchExternalContext::new(functions))
        .append_handler_register(register_bench_handler)
        .build()
}

fn build_plain_evm<'a, DB: revm::Database + 'static>(db: DB) -> Evm<'a, (), DB> {
    revm::Evm::builder().with_db(db).build()
}

fn register_bench_handler<DB: revm::Database + 'static>(
    handler: &mut EvmHandler<'_, BenchExternalContext, DB>,
) {
    let prev = handler.execution.execute_frame.clone();
    handler.execution.execute_frame = Arc::new(move |frame, memory, tables, context| {
        let interpreter = frame.interpreter_mut();
        let bytecode_hash = interpreter.contract.hash.unwrap_or_default();
        if let Some(f) = context.external.get_function(bytecode_hash) {
            Ok(unsafe { f.call_with_interpreter_and_memory(interpreter, memory, context) })
        } else {
            prev(frame, memory, tables, context)
        }
    });
}
