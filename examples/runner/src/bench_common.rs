// Shared benchmark infrastructure for JSON fixture-based EVM benchmarks.
//
// Provides fixture loading, JIT compilation, and EVM execution helpers
// used by bench_json_test, bench_burntpix, and bench_curve.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use k256::ecdsa::SigningKey;
use revm::{
    bytecode::Bytecode,
    context::{BlockEnv, CfgEnv, TxEnv},
    context_interface::{
        result::{EVMError, HaltReason, InvalidTransaction, ResultAndState},
        ContextSetters,
    },
    database::{CacheDB, EmptyDB},
    handler::{EvmTr, FrameResult, Handler, ItemOrResult, MainBuilder},
    primitives::{
        hardfork::SpecId, keccak256, Address, Bytes, HashMap as RevmHashMap, StorageKey,
        StorageValue, TxKind, B256, U256,
    },
    state::AccountInfo,
    ExecuteEvm, MainnetEvm,
};
use revmc::{EvmCompiler, EvmLlvmBackend, OptimizationLevel};
use revmc_context::{EvmCompilerFn, RawEvmCompilerFn};
use serde::Deserialize;

// -- Public API ---------------------------------------------------------------

/// Pre-parsed and pre-compiled benchmark fixture ready for repeated execution.
pub struct Fixture {
    block: BlockEnv,
    cfg: CfgEnv,
    tx: TxEnv,
    compiled: CompiledContracts,
    prebuilt_db: Arc<CacheDB<EmptyDB>>,
}

impl Fixture {
    /// Load a JSON fixture from `fixture_relative_path` (relative to repo root).
    pub fn load(fixture_relative_path: &str) -> Result<Self, String> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join(fixture_relative_path);
        Self::from_path(&path)
    }

    /// Build a fixture from raw bytecode and calldata (no JSON file needed).
    ///
    /// Deploys the bytecode to a synthetic contract address and creates a
    /// minimal CANCUN environment that calls it with the given calldata.
    pub fn from_bytecode(bytecode: &[u8], calldata: &[u8]) -> Result<Self, String> {
        let contract_addr: Address = "0x1000000000000000000000000000000000000001"
            .parse()
            .unwrap();
        let caller: Address = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
            .parse()
            .unwrap();

        let code = Bytecode::new_raw(Bytes::from(bytecode.to_vec()));
        let code_hash = code.hash_slow();

        let info = AccountInfo {
            balance: U256::from(1_000_000_000_000_000_000u128),
            nonce: 0,
            code_hash,
            code: Some(code),
        };

        let accounts = vec![PreparedAccount {
            address: contract_addr,
            info,
            storage: Default::default(),
        }];

        let compiled = compile_contracts(&accounts)?;

        let mut db = CacheDB::new(EmptyDB::new());
        db.insert_account_info(contract_addr, accounts[0].info.clone());
        // Fund the caller so the transaction doesn't fail
        db.insert_account_info(
            caller,
            AccountInfo {
                balance: U256::from(10_000_000_000_000_000_000u128),
                nonce: 0,
                code_hash: revm::primitives::KECCAK_EMPTY,
                code: None,
            },
        );

        let cfg = CfgEnv::new_with_spec(SpecId::CANCUN);
        let mut block = BlockEnv::default();
        block.number = U256::from(1);
        block.timestamp = U256::from(1);
        block.gas_limit = 1_000_000_000;
        block.basefee = 1;

        let tx = TxEnv {
            tx_type: 0,
            caller,
            gas_limit: 1_000_000_000,
            gas_price: 1,
            kind: TxKind::Call(contract_addr),
            value: U256::ZERO,
            data: Bytes::from(calldata.to_vec()),
            nonce: 0,
            chain_id: Some(cfg.chain_id),
            access_list: Default::default(),
            gas_priority_fee: None,
            blob_hashes: Vec::new(),
            max_fee_per_blob_gas: 0,
            authorization_list: Vec::new(),
        };

        Ok(Self {
            block,
            cfg,
            tx,
            compiled,
            prebuilt_db: Arc::new(db),
        })
    }

    /// Build a fixture from a pre-populated CacheDB and transaction.
    ///
    /// Scans the DB for contract bytecodes and JIT-compiles them.
    pub fn from_db(db: CacheDB<EmptyDB>, block: BlockEnv, tx: TxEnv) -> Result<Self, String> {
        let accounts: Vec<PreparedAccount> = db
            .cache
            .accounts
            .iter()
            .map(|(addr, cached)| PreparedAccount {
                address: *addr,
                info: cached.info.clone(),
                storage: Default::default(),
            })
            .collect();
        let compiled = compile_contracts(&accounts)?;
        let cfg = CfgEnv::new_with_spec(SpecId::CANCUN);
        Ok(Self { block, cfg, tx, compiled, prebuilt_db: Arc::new(db) })
    }

    /// Run plain (interpreter-only) EVM execution.
    pub fn run_plain(&self) -> Result<ResultAndState, String> {
        let mut evm = self.make_plain_evm();
        evm.transact(self.tx.clone())
            .map_err(|e| format!("Plain execution failed: {:?}", e))
    }

    /// Access the pre-built CacheDB.
    pub fn prebuilt_db(&self) -> &Arc<CacheDB<EmptyDB>> {
        &self.prebuilt_db
    }

    /// Access the block environment.
    pub fn block(&self) -> &BlockEnv {
        &self.block
    }

    /// Access the cfg environment.
    pub fn cfg(&self) -> &CfgEnv {
        &self.cfg
    }

    /// Access the transaction environment.
    pub fn tx(&self) -> &TxEnv {
        &self.tx
    }

    /// Run JIT-compiled EVM execution (with interpreter fallback for non-JIT contracts).
    pub fn run_jit(&self) -> Result<ResultAndState, String> {
        let mut evm = self.make_plain_evm();
        let tx = self.tx.clone();
        // Set the transaction on the context so the handler can validate/execute it
        evm.ctx.set_tx(tx);

        let mut handler = JitHandler {
            functions: self.compiled.functions.clone(),
        };
        let result = handler
            .run(&mut evm)
            .map_err(|e| format!("JIT execution failed: {:?}", e))?;
        let state = evm.ctx.journaled_state.finalize();
        Ok(ResultAndState::new(result, state))
    }
}

// -- Internal -----------------------------------------------------------------

impl Fixture {
    fn from_path(path: &Path) -> Result<Self, String> {
        let json = fs::read_to_string(path).map_err(|err| err.to_string())?;
        let test_file: RawTestFile =
            serde_json::from_str(&json).map_err(|err| err.to_string())?;
        let mut cases = test_file.cases.into_values();
        let raw_case = cases
            .next()
            .ok_or_else(|| "fixture does not contain any test cases".to_owned())?;
        let first_tx = raw_case
            .transaction
            .into_iter()
            .next()
            .ok_or_else(|| "fixture does not contain any transactions".to_owned())?;

        let mut block = build_block_env(&raw_case.env)?;
        let cfg = CfgEnv::new_with_spec(SpecId::CANCUN);
        let tx = build_tx_env(&cfg, &first_tx)?;
        // Ensure block gas limit accommodates the transaction
        if block.gas_limit < tx.gas_limit {
            block.gas_limit = tx.gas_limit;
        }
        let accounts = parse_accounts(raw_case.pre)?;
        let compiled = compile_contracts(&accounts)?;

        // Prebuild DB once to avoid repeated construction in benchmark iterations
        let mut db = CacheDB::new(EmptyDB::new());
        for account in &accounts {
            db.insert_account_info(account.address, account.info.clone());
            if !account.storage.is_empty() {
                db.replace_account_storage(account.address, account.storage.clone())
                    .map_err(|e| format!("failed to populate account storage: {:?}", e))?;
            }
        }
        let prebuilt_db = Arc::new(db);

        Ok(Self {
            block,
            cfg,
            tx,
            compiled,
            prebuilt_db,
        })
    }

    fn make_plain_evm(&self) -> BenchEvm<'static> {
        // SAFETY: we treat the Arc's inner data as exclusively owned per iteration.
        // The benchmark is single-threaded and each iteration is independent.
        let db_ref =
            unsafe { &mut *(Arc::as_ptr(&self.prebuilt_db) as *mut CacheDB<EmptyDB>) };
        let ctx = revm::context::Context::new(db_ref, SpecId::CANCUN);
        let mut evm = ctx.build_mainnet();
        evm.ctx.block = self.block.clone();
        evm.ctx.cfg = self.cfg.clone();
        evm
    }
}

// -- JIT Handler --------------------------------------------------------------

type BenchEvm<'a> = MainnetEvm<revm::handler::MainnetContext<&'a mut CacheDB<EmptyDB>>>;
type BenchError = EVMError<core::convert::Infallible, InvalidTransaction>;

pub struct JitHandler {
    pub functions: Arc<HashMap<B256, RawEvmCompilerFn>>,
}

impl Handler for JitHandler {
    type Evm = BenchEvm<'static>;
    type Error = BenchError;
    type HaltReason = HaltReason;

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
                let frame = evm.frame_stack.get();
                let bytecode_hash = frame.interpreter.bytecode.get_or_calculate_hash();

                if let Some(&raw_fn) = self.functions.get(&bytecode_hash) {
                    let ctx = &mut evm.ctx;
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
                    // Fall back to standard frame_run for non-JIT contracts.
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

// -- JSON fixture parsing -----------------------------------------------------

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

// -- Environment builders -----------------------------------------------------

fn build_block_env(raw: &RawEnv) -> Result<BlockEnv, String> {
    let mut block = BlockEnv::default();
    block.number = parse_u256(
        raw.current_number
            .as_deref()
            .ok_or_else(|| "missing env.currentNumber".to_owned())?,
    )?;
    block.timestamp = parse_u256(
        raw.current_timestamp
            .as_deref()
            .ok_or_else(|| "missing env.currentTimestamp".to_owned())?,
    )?;
    block.gas_limit = parse_u64(
        raw.current_gas_limit
            .as_deref()
            .ok_or_else(|| "missing env.currentGasLimit".to_owned())?,
    )?;
    block.basefee = parse_u64(
        raw.current_base_fee
            .as_deref()
            .ok_or_else(|| "missing env.currentBaseFee".to_owned())?,
    )?;
    block.beneficiary = parse_address(
        raw.current_coinbase
            .as_deref()
            .ok_or_else(|| "missing env.currentCoinbase".to_owned())?,
    )?;
    if let Some(difficulty) = &raw.current_difficulty {
        block.difficulty = parse_u256(difficulty)?;
    }
    block.prevrandao = match raw.current_random.as_deref() {
        Some(value) => Some(parse_b256(value)?),
        None => None,
    };
    match raw.current_excess_blob_gas.as_deref() {
        Some(value) => {
            let excess = parse_u64(value)?;
            block.set_blob_excess_gas_and_price(
                excess,
                revm::primitives::eip4844::BLOB_BASE_FEE_UPDATE_FRACTION_CANCUN,
            );
        }
        None => {
            // CANCUN requires blob_excess_gas_and_price; default to 0 if fixture omits it.
            block.set_blob_excess_gas_and_price(
                0,
                revm::primitives::eip4844::BLOB_BASE_FEE_UPDATE_FRACTION_CANCUN,
            );
        }
    }
    Ok(block)
}

fn build_tx_env(cfg: &CfgEnv, tx: &RawTransaction) -> Result<TxEnv, String> {
    let caller = derive_caller_address(&tx.secret_key)?;
    let gas_limit = parse_u64(&tx.gas_limit)?;

    let gas_price_source = tx
        .gas_price
        .as_deref()
        .or(tx.max_fee_per_gas.as_deref())
        .unwrap_or("0x0");
    let gas_price = parse_u128(gas_price_source)?;

    let gas_priority_fee = match tx.max_priority_fee_per_gas.as_deref() {
        Some(value) => Some(parse_u128(value)?),
        None => None,
    };

    let value = parse_u256(tx.value.as_deref().unwrap_or("0x0"))?;
    let data = parse_bytes(&tx.data)?;
    let nonce = parse_u64(tx.nonce.as_deref().unwrap_or("0x0"))?;

    let kind = match tx.to.as_deref() {
        Some(value) if value.trim().is_empty() || value.trim() == "0x" => TxKind::Create,
        Some(value) => TxKind::Call(parse_address(value)?),
        None => TxKind::Create,
    };

    let blob_hashes = tx
        .blob_hashes
        .iter()
        .map(|hash| parse_b256(hash))
        .collect::<Result<Vec<_>, _>>()?;
    let max_fee_per_blob_gas = match tx.max_fee_per_blob_gas.as_deref() {
        Some(value) => parse_u128(value)?,
        None => 0,
    };

    Ok(TxEnv {
        tx_type: 0, // will be derived
        caller,
        gas_limit,
        gas_price,
        kind,
        value,
        data,
        nonce,
        chain_id: Some(cfg.chain_id),
        access_list: Default::default(),
        gas_priority_fee,
        blob_hashes,
        max_fee_per_blob_gas,
        authorization_list: Vec::new(),
    })
}

// -- Account parsing ----------------------------------------------------------

#[derive(Clone)]
struct PreparedAccount {
    address: Address,
    info: AccountInfo,
    storage: RevmHashMap<StorageKey, StorageValue>,
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

    let info = AccountInfo {
        balance,
        nonce,
        code_hash,
        code: Some(bytecode),
    };

    Ok(PreparedAccount {
        address,
        info,
        storage,
    })
}

fn parse_storage(
    storage: BTreeMap<String, String>,
) -> Result<RevmHashMap<StorageKey, StorageValue>, String> {
    let mut entries: RevmHashMap<StorageKey, StorageValue> =
        RevmHashMap::with_capacity_and_hasher(storage.len(), Default::default());
    for (slot_hex, value_hex) in storage {
        let slot = parse_u256(&slot_hex)?;
        let value = parse_u256(&value_hex)?;
        entries.insert(slot, value);
    }
    Ok(entries)
}

// -- JIT compilation ----------------------------------------------------------

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
        let Some(code) = account.info.code.as_ref() else {
            continue;
        };
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
        let fn_ptr =
            unsafe { compiler.jit_function(func_id).map_err(|err| err.to_string())? };
        functions.insert(hash, fn_ptr.into_inner());
    }

    Ok(CompiledContracts {
        functions: Arc::new(functions),
        _compiler: compiler,
        _context: context,
    })
}

// -- Hex parsing helpers ------------------------------------------------------

fn derive_caller_address(secret_hex: &str) -> Result<Address, String> {
    let raw = parse_fixed_bytes(secret_hex, 32)?;
    let key_bytes: [u8; 32] = raw
        .try_into()
        .map_err(|_| "secret key must be 32 bytes".to_owned())?;
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

fn parse_u128(value: &str) -> Result<u128, String> {
    let trimmed = strip_0x(value.trim());
    if trimmed.is_empty() {
        Ok(0)
    } else {
        u128::from_str_radix(trimmed, 16).map_err(|err| err.to_string())
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
        return Err(format!(
            "value {value} exceeds expected length of {expected_len} bytes"
        ));
    }
    if bytes.len() < expected_len {
        let mut padded = vec![0u8; expected_len - bytes.len()];
        padded.extend_from_slice(&bytes);
        bytes = padded;
    }
    Ok(bytes)
}

fn strip_0x(value: &str) -> &str {
    value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value)
}
