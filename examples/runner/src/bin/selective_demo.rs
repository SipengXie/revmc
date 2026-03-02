// Selective JIT Synthetic Benchmark
//
// Demonstrates that compiling only compute-heavy contracts (Uniswap V2 Router)
// outperforms both native interpretation and full JIT compilation.
//
// Workload: interleaved Uniswap swaps (compute-heavy) + ERC20 airdrops (storage-heavy).
// Three modes: Native (0 compiled), Full JIT (all compiled), Selective JIT (all except airdrop).

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use clap::Parser;
use revm::{
    bytecode::Bytecode,
    context::{BlockEnv, CfgEnv, TxEnv},
    context_interface::{
        result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction},
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

// ── Constants ────────────────────────────────────────────────────────────────

/// Uniswap V2 Router (0xff00...0022)
const ROUTER: Address = Address::new([
    0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x22,
]);

/// Input token for swaps (0x1100...0000, ERC20 in fixture)
const SWAP_INPUT_TOKEN: Address = Address::new([
    0x11, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
]);

/// Output token for swaps (0x1100...0050, paired token in fixture)
#[allow(dead_code)]
const SWAP_OUTPUT_TOKEN: Address = Address::new([
    0x11, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x50,
]);

/// Standalone airdrop ERC20 token (not in fixture, deployed at 0xaaaa...aa)
const AIRDROP_TOKEN: Address = Address::new([0xaa; 20]);

/// Airdrop contract
const AIRDROP: Address = Address::new([0xdd; 20]);

/// Sender EOA for airdrop transactions
const SENDER: Address = Address::new([
    0x89, 0xd5, 0xe7, 0x2a, 0x8a, 0x4a, 0x03, 0x30, 0xa6, 0x5b, 0xbc, 0xef, 0x30, 0x32, 0xbe,
    0x2f, 0x72, 0x82, 0x64, 0xa8,
]);

// ── CLI ──────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "selective_demo", about = "Selective JIT advantage demonstration")]
struct Args {
    #[arg(long, default_value_t = 500)]
    swap_txs: usize,

    #[arg(long, default_value_t = 500)]
    airdrop_txs: usize,

    #[arg(long, default_value_t = 2000)]
    recipients_per_airdrop: usize,

    #[arg(long, default_value_t = 5)]
    rounds: usize,

    #[arg(long, default_value_t = 1)]
    warmup: usize,

    /// Print per-tx gas info on first round
    #[arg(long)]
    verbose: bool,
}

// ── EVM types ────────────────────────────────────────────────────────────────

type BenchEvm<'a> = MainnetEvm<revm::handler::MainnetContext<&'a mut CacheDB<EmptyDB>>>;
type BenchError = EVMError<core::convert::Infallible, InvalidTransaction>;

// ── JIT Handler ──────────────────────────────────────────────────────────────

struct JitHandler {
    functions: Arc<HashMap<B256, RawEvmCompilerFn>>,
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

// ── Fixture loading ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct FixtureFile {
    #[serde(flatten)]
    cases: BTreeMap<String, FixtureCase>,
}

#[derive(Deserialize)]
struct FixtureCase {
    pre: BTreeMap<String, RawAccount>,
    env: FixtureEnv,
}

#[derive(Deserialize)]
struct FixtureEnv {
    #[serde(rename = "currentBaseFee")]
    current_base_fee: String,
    #[serde(rename = "currentCoinbase")]
    current_coinbase: String,
    #[serde(rename = "currentGasLimit")]
    current_gas_limit: String,
    #[serde(rename = "currentNumber")]
    current_number: String,
    #[serde(rename = "currentTimestamp")]
    current_timestamp: String,
    #[serde(rename = "currentRandom")]
    current_random: String,
}

#[derive(Deserialize)]
struct RawAccount {
    balance: String,
    code: String,
    nonce: String,
    #[serde(default)]
    storage: BTreeMap<String, String>,
}

struct ContractCode {
    address: Address,
    code_hash: B256,
    bytecode: Bytecode,
}

fn load_fixture_accounts(db: &mut CacheDB<EmptyDB>) -> (Vec<ContractCode>, FixtureEnv) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../data/uniswap-t100-c20.json");
    let json = fs::read_to_string(&path).expect("failed to read fixture");
    let file: FixtureFile = serde_json::from_str(&json).expect("failed to parse fixture");
    let case = file.cases.into_values().next().expect("no cases in fixture");

    let mut contracts = Vec::new();

    for (addr_hex, raw) in &case.pre {
        let address = parse_address(addr_hex);
        let balance = parse_u256(&raw.balance);
        let nonce = parse_u64(&raw.nonce);
        let bytecode_bytes = parse_hex_bytes(&raw.code);
        let bytecode = Bytecode::new_raw(Bytes::from(bytecode_bytes));
        let code_hash = bytecode.hash_slow();

        let storage: RevmHashMap<StorageKey, StorageValue> = raw
            .storage
            .iter()
            .map(|(k, v)| (parse_u256(k), parse_u256(v)))
            .collect();

        let info = AccountInfo {
            balance,
            nonce,
            code_hash,
            code: Some(bytecode.clone()),
        };
        db.insert_account_info(address, info);
        if !storage.is_empty() {
            db.replace_account_storage(address, storage).unwrap();
        }

        // Track contracts with non-empty bytecode
        if !bytecode.is_empty() {
            contracts.push(ContractCode {
                address,
                code_hash,
                bytecode,
            });
        }
    }

    (contracts, case.env)
}

fn build_block_env(env: &FixtureEnv) -> BlockEnv {
    let mut block = BlockEnv::default();
    block.number = parse_u256(&env.current_number);
    block.beneficiary = parse_address(&env.current_coinbase);
    block.timestamp = parse_u256(&env.current_timestamp);
    block.gas_limit = parse_u64(&env.current_gas_limit);
    block.basefee = parse_u64(&env.current_base_fee);
    block.prevrandao =
        Some(B256::from_slice(&parse_fixed_bytes(&env.current_random, 32)));
    block
}

fn load_airdrop_bytecode() -> Bytecode {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/airdrop.rt.hex");
    let hex_str = fs::read_to_string(&path)
        .expect("failed to read airdrop.rt.hex")
        .trim()
        .to_string();
    let hex_str = hex_str.strip_prefix("0x").unwrap_or(&hex_str);
    let bytes = hex::decode(hex_str).expect("invalid hex in airdrop.rt.hex");
    Bytecode::new_raw(Bytes::from(bytes))
}

// ── ERC20 storage slot helpers ───────────────────────────────────────────────

/// Compute `balanceOf[addr]` slot for an ERC20 with balanceOf mapping at slot 0.
fn erc20_balance_slot(addr: Address) -> U256 {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(addr.as_slice());
    // slot 0 in buf[32..64] is already zero
    U256::from_be_bytes(keccak256(buf).0)
}

/// Compute `allowance[owner][spender]` slot for an ERC20 with allowance mapping at slot 1.
fn erc20_allowance_slot(owner: Address, spender: Address) -> U256 {
    // inner = keccak256(abi.encode(owner, 1))
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(owner.as_slice());
    buf[63] = 1; // slot 1
    let inner = keccak256(buf);

    // outer = keccak256(abi.encode(spender, inner))
    let mut buf2 = [0u8; 64];
    buf2[12..32].copy_from_slice(spender.as_slice());
    buf2[32..64].copy_from_slice(inner.as_slice());
    U256::from_be_bytes(keccak256(buf2).0)
}

// ── Database setup ───────────────────────────────────────────────────────────

fn make_swap_caller(index: usize) -> Address {
    let mut bytes = [0u8; 20];
    bytes[0] = 0x20;
    bytes[16..20].copy_from_slice(&(index as u32).to_be_bytes());
    Address::from(bytes)
}

fn build_db(args: &Args) -> (CacheDB<EmptyDB>, Vec<ContractCode>, BlockEnv) {
    let mut db = CacheDB::new(EmptyDB::new());

    // Load Uniswap fixture (Router, Factory, Tokens, Pairs, EOAs)
    let (mut contracts, fixture_env) = load_fixture_accounts(&mut db);
    let block_env = build_block_env(&fixture_env);

    // Insert airdrop contract
    let airdrop_code = load_airdrop_bytecode();
    let airdrop_hash = airdrop_code.hash_slow();
    db.insert_account_info(
        AIRDROP,
        AccountInfo {
            balance: U256::ZERO,
            nonce: 1,
            code_hash: airdrop_hash,
            code: Some(airdrop_code.clone()),
        },
    );
    // Set airdrop owner = SENDER at OZ v5 ERC-7201 namespaced storage slot for Ownable._owner
    let oz_owner_slot = U256::from_str_radix(
        "9016d09d72d40fdae2fd8ceac6b6234c7706214fd39c1cd1e609a0528c199300",
        16,
    )
    .unwrap();
    db.insert_account_storage(AIRDROP, oz_owner_slot, U256::from_be_slice(SENDER.as_slice()))
        .unwrap();
    contracts.push(ContractCode {
        address: AIRDROP,
        code_hash: airdrop_hash,
        bytecode: airdrop_code,
    });

    // Deploy airdrop ERC20 token at AIRDROP_TOKEN (0xaaaa...aa)
    // Reuse bytecode from SWAP_INPUT_TOKEN (has ERC20 functionality)
    let token_bytecode = contracts
        .iter()
        .find(|c| c.address == SWAP_INPUT_TOKEN)
        .expect("SWAP_INPUT_TOKEN not found in fixture")
        .bytecode
        .clone();
    let token_hash = token_bytecode.hash_slow();
    db.insert_account_info(
        AIRDROP_TOKEN,
        AccountInfo {
            balance: U256::ZERO,
            nonce: 1,
            code_hash: token_hash,
            code: Some(token_bytecode.clone()),
        },
    );
    contracts.push(ContractCode {
        address: AIRDROP_TOKEN,
        code_hash: token_hash,
        bytecode: token_bytecode,
    });

    // Fund SENDER for airdrops: ETH + AIRDROP_TOKEN balance + AIRDROP_TOKEN→AIRDROP allowance
    let huge = U256::from(10u64).pow(U256::from(30));
    db.insert_account_info(
        SENDER,
        AccountInfo {
            balance: U256::from(10u64).pow(U256::from(25)), // 10M ETH
            nonce: 0,
            code_hash: revm::primitives::KECCAK_EMPTY,
            code: None,
        },
    );
    db.insert_account_storage(AIRDROP_TOKEN, erc20_balance_slot(SENDER), huge)
        .unwrap();
    db.insert_account_storage(
        AIRDROP_TOKEN,
        erc20_allowance_slot(SENDER, AIRDROP),
        U256::MAX,
    )
    .unwrap();

    // Generate swap callers and fund each with:
    //   - ETH for gas
    //   - SWAP_INPUT_TOKEN balance
    //   - SWAP_INPUT_TOKEN → ROUTER allowance
    let caller_token_balance = U256::from(10u64).pow(U256::from(21)); // 1000 tokens
    for i in 0..args.swap_txs {
        let caller = make_swap_caller(i);
        db.insert_account_info(
            caller,
            AccountInfo {
                balance: U256::from(10u64).pow(U256::from(20)), // 100 ETH
                nonce: 0,
                code_hash: revm::primitives::KECCAK_EMPTY,
                code: None,
            },
        );
        db.insert_account_storage(
            SWAP_INPUT_TOKEN,
            erc20_balance_slot(caller),
            caller_token_balance,
        )
        .unwrap();
        db.insert_account_storage(
            SWAP_INPUT_TOKEN,
            erc20_allowance_slot(caller, ROUTER),
            U256::MAX,
        )
        .unwrap();
    }

    println!(
        "  accounts: {} | contracts: {} unique bytecodes",
        db.cache.accounts.len(),
        contracts.len()
    );
    (db, contracts, block_env)
}

// ── Transaction generation ───────────────────────────────────────────────────

fn make_swap_calldata(amount_in: U256, path: &[Address], to: Address, deadline: U256) -> Bytes {
    // swapExactTokensForTokens(uint256,uint256,address[],address,uint256)
    // selector: 0x38ed1739
    let mut data = Vec::with_capacity(4 + (5 + 1 + path.len()) * 32);
    data.extend_from_slice(&[0x38, 0xed, 0x17, 0x39]);

    // amountIn
    data.extend_from_slice(&amount_in.to_be_bytes::<32>());
    // amountOutMin = 0
    data.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
    // offset to path array (5 static words * 32 = 160)
    data.extend_from_slice(&U256::from(5u64 * 32).to_be_bytes::<32>());
    // to
    let mut padded = [0u8; 32];
    padded[12..32].copy_from_slice(to.as_slice());
    data.extend_from_slice(&padded);
    // deadline
    data.extend_from_slice(&deadline.to_be_bytes::<32>());

    // path array
    data.extend_from_slice(&U256::from(path.len()).to_be_bytes::<32>());
    for addr in path {
        let mut p = [0u8; 32];
        p[12..32].copy_from_slice(addr.as_slice());
        data.extend_from_slice(&p);
    }

    Bytes::from(data)
}

fn make_airdrop_calldata(token: Address, recipients: &[Address], amount_each: U256) -> Bytes {
    // airdropERC20(address, address[], uint256[], uint256) — selector 0xccb98ffc
    let n = recipients.len();
    let total = amount_each * U256::from(n);

    let mut data = Vec::with_capacity(4 + 4 * 32 + 2 * (32 + n * 32));
    data.extend_from_slice(&[0xcc, 0xb9, 0x8f, 0xfc]);

    // param 0: token address (static)
    let mut padded = [0u8; 32];
    padded[12..32].copy_from_slice(token.as_slice());
    data.extend_from_slice(&padded);

    // param 1: offset to recipients array
    data.extend_from_slice(&U256::from(4 * 32).to_be_bytes::<32>());

    // param 2: offset to amounts array
    let recipients_section = 32 + n * 32; // length + n elements
    data.extend_from_slice(&U256::from(4 * 32 + recipients_section).to_be_bytes::<32>());

    // param 3: totalAmount (static)
    data.extend_from_slice(&total.to_be_bytes::<32>());

    // recipients array
    data.extend_from_slice(&U256::from(n).to_be_bytes::<32>());
    for &r in recipients {
        let mut p = [0u8; 32];
        p[12..32].copy_from_slice(r.as_slice());
        data.extend_from_slice(&p);
    }

    // amounts array
    data.extend_from_slice(&U256::from(n).to_be_bytes::<32>());
    for _ in 0..n {
        data.extend_from_slice(&amount_each.to_be_bytes::<32>());
    }

    Bytes::from(data)
}

fn generate_txs(args: &Args, basefee: u64) -> Vec<TxEnv> {
    let gas_price = basefee as u128;
    let swap_amount = U256::from(1_000_000_000_000_000u64); // 0.001 token per swap
    let path = [SWAP_INPUT_TOKEN, SWAP_OUTPUT_TOKEN];
    let deadline = U256::from(u64::MAX);

    // Build per-caller swap calldata (each caller receives output to themselves)
    let swap_calldatas: Vec<Bytes> = (0..args.swap_txs)
        .map(|i| {
            let caller = make_swap_caller(i);
            make_swap_calldata(swap_amount, &path, caller, deadline)
        })
        .collect();

    // Pre-generate all recipient addresses (unique per airdrop batch)
    let total_recipients = args.airdrop_txs * args.recipients_per_airdrop;
    let recipients: Vec<Address> = (0..total_recipients)
        .map(|i| {
            let mut bytes = [0u8; 20];
            bytes[0] = 0x10;
            bytes[16..20].copy_from_slice(&(i as u32).to_be_bytes());
            Address::from(bytes)
        })
        .collect();

    let amount_each = U256::from(1_000_000_000_000_000u128); // 0.001 token per recipient

    // Pre-build all airdrop calldata batches
    let airdrop_calldatas: Vec<Bytes> = (0..args.airdrop_txs)
        .map(|i| {
            let start = i * args.recipients_per_airdrop;
            let end = start + args.recipients_per_airdrop;
            make_airdrop_calldata(AIRDROP_TOKEN, &recipients[start..end], amount_each)
        })
        .collect();

    let mut txs = Vec::with_capacity(args.swap_txs + args.airdrop_txs);
    let mut swap_idx = 0;
    let mut airdrop_idx = 0;
    // All txs use nonce=0 since each executes on a fresh DB snapshot (no state accumulation).
    let nonce = 0u64;

    // Interleave: one swap, one airdrop, repeat
    while swap_idx < args.swap_txs || airdrop_idx < args.airdrop_txs {
        if swap_idx < args.swap_txs {
            let caller = make_swap_caller(swap_idx);
            txs.push(TxEnv {
                tx_type: 0,
                caller,
                gas_limit: 1_000_000,
                gas_price,
                kind: TxKind::Call(ROUTER),
                value: U256::ZERO,
                data: swap_calldatas[swap_idx].clone(),
                nonce,
                chain_id: Some(1),
                access_list: Default::default(),
                gas_priority_fee: None,
                blob_hashes: Vec::new(),
                max_fee_per_blob_gas: 0,
                authorization_list: Vec::new(),
            });
            swap_idx += 1;
        }
        if airdrop_idx < args.airdrop_txs {
            txs.push(TxEnv {
                tx_type: 0,
                caller: SENDER,
                gas_limit: 100_000_000,
                gas_price,
                kind: TxKind::Call(AIRDROP),
                value: U256::ZERO,
                data: airdrop_calldatas[airdrop_idx].clone(),
                nonce,
                chain_id: Some(1),
                access_list: Default::default(),
                gas_priority_fee: None,
                blob_hashes: Vec::new(),
                max_fee_per_blob_gas: 0,
                authorization_list: Vec::new(),
            });
            airdrop_idx += 1;
        }
    }

    txs
}

// ── JIT compilation ──────────────────────────────────────────────────────────

struct CompiledContracts {
    full_functions: Arc<HashMap<B256, RawEvmCompilerFn>>,
    selective_functions: Arc<HashMap<B256, RawEvmCompilerFn>>,
    #[allow(dead_code)]
    _compiler: &'static mut EvmCompiler<EvmLlvmBackend<'static>>,
    #[allow(dead_code)]
    _context: &'static revmc::llvm::inkwell::context::Context,
}

fn compile_all(contracts: &[ContractCode]) -> CompiledContracts {
    let context = Box::leak(Box::new(revmc::llvm::inkwell::context::Context::create()));
    let backend =
        EvmLlvmBackend::new(context, false, OptimizationLevel::Aggressive).expect("LLVM backend");
    let compiler: &'static mut EvmCompiler<EvmLlvmBackend<'static>> =
        Box::leak(Box::new(EvmCompiler::new(backend)));

    let mut seen = HashSet::new();
    let mut pending = Vec::new();

    for c in contracts {
        if c.bytecode.is_empty() || !seen.insert(c.code_hash) {
            continue;
        }
        let name = format!("contract_{}", hex::encode(c.code_hash.as_slice()));
        let func_id = compiler
            .translate(&name, c.bytecode.original_byte_slice(), SpecId::CANCUN)
            .expect("translation failed");
        pending.push((c.address, c.code_hash, func_id));
    }

    let mut full_map = HashMap::new();
    let mut airdrop_hash = B256::ZERO;

    for (addr, hash, func_id) in pending {
        let fn_ptr = unsafe { compiler.jit_function(func_id).expect("JIT failed") };
        full_map.insert(hash, fn_ptr.into_inner());
        if addr == AIRDROP {
            airdrop_hash = hash;
        }
    }

    // Selective map: everything except airdrop contract
    let selective_map: HashMap<B256, RawEvmCompilerFn> = full_map
        .iter()
        .filter(|(h, _)| **h != airdrop_hash)
        .map(|(h, f)| (*h, *f))
        .collect();

    println!(
        "  full: {} unique JIT functions | selective: {} (all except airdrop)",
        full_map.len(),
        selective_map.len()
    );

    CompiledContracts {
        full_functions: Arc::new(full_map),
        selective_functions: Arc::new(selective_map),
        _compiler: compiler,
        _context: context,
    }
}

// ── Execution ────────────────────────────────────────────────────────────────

fn execute_round(
    template_db: &Arc<CacheDB<EmptyDB>>,
    txs: &[TxEnv],
    functions: &Arc<HashMap<B256, RawEvmCompilerFn>>,
    block: &BlockEnv,
    verbose: bool,
) -> (Duration, usize, usize) {
    let cfg = CfgEnv::new_with_spec(SpecId::CANCUN);
    let is_native = functions.is_empty();

    let mut successes = 0usize;
    let mut reverts = 0usize;

    // Create the EVM ONCE per round — reuse it across all txs.
    // Each tx runs on the same DB state (no commit between txs).
    // transact() finalizes the journal internally; for JIT we call finalize() manually.
    // SAFETY: single-threaded, no aliasing — only one EVM uses this reference.
    let db_ref = unsafe { &mut *(Arc::as_ptr(template_db) as *mut CacheDB<EmptyDB>) };
    let ctx = revm::context::Context::new(db_ref, SpecId::CANCUN);
    let mut evm = ctx.build_mainnet();
    evm.ctx.block = block.clone();
    evm.ctx.cfg = cfg;

    let t0 = Instant::now();

    for (i, tx) in txs.iter().enumerate() {
        let ok = if is_native {
            match evm.transact(tx.clone()) {
                Ok(ras) => {
                    if verbose && i < 6 {
                        log_result(i, tx, &ras.result);
                    }
                    ras.result.is_success()
                }
                Err(e) => {
                    if verbose && i < 6 {
                        println!("    tx[{i}] TX-LEVEL ERROR: {e:?}");
                    }
                    false
                }
            }
        } else {
            evm.ctx.set_tx(tx.clone());
            let mut handler = JitHandler {
                functions: functions.clone(),
            };
            let result = match handler.run(&mut evm) {
                Ok(result) => {
                    if verbose && i < 6 {
                        let gas = result.gas_used();
                        println!("    tx[{i}] gas={gas} ok={}", result.is_success());
                    }
                    result.is_success()
                }
                Err(e) => {
                    if verbose && i < 6 {
                        println!("    tx[{i}] ERROR: {e:?}");
                    }
                    false
                }
            };
            // Clear journal for next tx (transact does this internally for native)
            let _ = evm.ctx.journaled_state.finalize();
            result
        };
        if ok {
            successes += 1;
        } else {
            reverts += 1;
        }
    }

    (t0.elapsed(), successes, reverts)
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn decode_revert_reason(output: &Bytes) -> String {
    if output.is_empty() {
        return "empty".into();
    }
    // Try to decode Solidity Error(string): 0x08c379a0 + abi.encode(string)
    if output.len() >= 68 && output[..4] == [0x08, 0xc3, 0x79, 0xa0] {
        let offset_u256 = U256::from_be_slice(&output[4..36]);
        if let Ok(offset) = usize::try_from(offset_u256) {
            if 36 + offset + 32 <= output.len() {
                let len_u256 = U256::from_be_slice(&output[36 + offset..36 + offset + 32]);
                if let Ok(len) = usize::try_from(len_u256) {
                    if 36 + offset + 32 + len <= output.len() {
                        if let Ok(msg) = std::str::from_utf8(
                            &output[36 + offset + 32..36 + offset + 32 + len],
                        ) {
                            return msg.to_string();
                        }
                    }
                }
            }
        }
    }
    // Show full hex for custom errors
    format!("0x{} ({} bytes)", hex::encode(output.as_ref()), output.len())
}

fn log_result(i: usize, tx: &TxEnv, result: &ExecutionResult<HaltReason>) {
    let gas = result.gas_used();
    let to = match &tx.kind {
        TxKind::Call(a) => format!("{a}"),
        _ => "create".into(),
    };
    if result.is_success() {
        println!("    tx[{i}] to={to} gas={gas} ok=true");
    } else {
        let reason = match result {
            ExecutionResult::Revert { output, .. } => decode_revert_reason(output),
            ExecutionResult::Halt { reason, .. } => format!("{reason:?}"),
            _ => "unknown".into(),
        };
        println!("    tx[{i}] to={to} gas={gas} REVERT: {reason}");
    }
}

fn median(times: &mut [Duration]) -> Duration {
    times.sort();
    times[times.len() / 2]
}

// ── Main ─────────────────────────────────────────────────────────────────────

fn main() {
    let args = Args::parse();

    println!("=== Selective JIT Synthetic Benchmark ===");
    println!(
        "Uniswap swaps: {} | Airdrop txs: {} ({} recipients each)",
        args.swap_txs, args.airdrop_txs, args.recipients_per_airdrop
    );
    println!("Rounds: {} | Warmup: {}", args.rounds, args.warmup);
    println!();

    // Build template DB
    println!("[1/4] Building state...");
    let (template_db, contracts, block_env) = build_db(&args);
    let template_db = Arc::new(template_db);

    // Generate transactions
    println!(
        "[2/4] Generating {} transactions...",
        args.swap_txs + args.airdrop_txs
    );
    let txs = generate_txs(&args, block_env.basefee);

    // Compile contracts
    println!("[3/4] Compiling contracts...");
    let compiled = compile_all(&contracts);
    println!();

    // Benchmark modes
    struct Mode {
        name: &'static str,
        functions: Arc<HashMap<B256, RawEvmCompilerFn>>,
    }

    let modes = vec![
        Mode {
            name: "Native",
            functions: Arc::new(HashMap::new()),
        },
        Mode {
            name: "Full JIT",
            functions: compiled.full_functions.clone(),
        },
        Mode {
            name: "Selective JIT",
            functions: compiled.selective_functions.clone(),
        },
    ];

    println!("[4/4] Benchmarking...");
    println!();

    let mut results = Vec::new();

    for mode in &modes {
        let n_compiled = mode.functions.len();
        println!("--- {} ({} JIT functions) ---", mode.name, n_compiled);

        // Warmup
        for w in 0..args.warmup {
            let (elapsed, ok, err) = execute_round(
                &template_db,
                &txs,
                &mode.functions,
                &block_env,
                args.verbose && w == 0,
            );
            println!(
                "  warmup {}: {:.1}ms ({} ok, {} revert)",
                w + 1,
                elapsed.as_secs_f64() * 1000.0,
                ok,
                err
            );
        }

        // Measured rounds
        let mut times = Vec::with_capacity(args.rounds);
        for r in 0..args.rounds {
            let (elapsed, ok, err) =
                execute_round(&template_db, &txs, &mode.functions, &block_env, false);
            println!(
                "  round {}: {:.1}ms ({} ok, {} revert)",
                r + 1,
                elapsed.as_secs_f64() * 1000.0,
                ok,
                err
            );
            times.push(elapsed);
        }

        let med = median(&mut times);
        results.push((mode.name, n_compiled, med));
        println!("  median: {:.1}ms", med.as_secs_f64() * 1000.0);
        println!();
    }

    // Report
    println!("=== Results ===");
    println!(
        "{:<15} {:>10} {:>12} {:>10}",
        "Mode", "JIT Fns", "Time(ms)", "Speedup"
    );
    println!("{}", "-".repeat(50));

    let native_ms = results[0].2.as_secs_f64() * 1000.0;
    for (name, n_compiled, med) in &results {
        let ms = med.as_secs_f64() * 1000.0;
        let speedup = if ms > 0.0 { native_ms / ms } else { 0.0 };
        println!(
            "{:<15} {:>10} {:>12.1} {:>9.2}x",
            name, n_compiled, ms, speedup
        );
    }
}

// ── Hex parsing helpers ──────────────────────────────────────────────────────

fn parse_address(value: &str) -> Address {
    let bytes = parse_fixed_bytes(value, 20);
    Address::from_slice(&bytes)
}

fn parse_u256(value: &str) -> U256 {
    let trimmed = strip_0x(value.trim());
    if trimmed.is_empty() {
        U256::ZERO
    } else {
        U256::from_str_radix(trimmed, 16).expect("invalid U256")
    }
}

fn parse_u64(value: &str) -> u64 {
    let trimmed = strip_0x(value.trim());
    if trimmed.is_empty() {
        0
    } else {
        u64::from_str_radix(trimmed, 16).expect("invalid u64")
    }
}

fn parse_hex_bytes(value: &str) -> Vec<u8> {
    let trimmed = strip_0x(value.trim());
    if trimmed.is_empty() {
        return Vec::new();
    }
    let even = if trimmed.len() % 2 == 0 {
        trimmed.to_owned()
    } else {
        format!("0{trimmed}")
    };
    hex::decode(even).expect("invalid hex")
}

fn parse_fixed_bytes(value: &str, expected_len: usize) -> Vec<u8> {
    let mut bytes = parse_hex_bytes(value);
    if bytes.len() < expected_len {
        let mut padded = vec![0u8; expected_len - bytes.len()];
        padded.extend_from_slice(&bytes);
        bytes = padded;
    }
    bytes
}

fn strip_0x(value: &str) -> &str {
    value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value)
}
