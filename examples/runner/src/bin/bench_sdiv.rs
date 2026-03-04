/// Synthetic micro-benchmark for SDIV/SMOD vs DIV/MOD JIT codegen quality.
///
/// Generates tight EVM loops containing only the target opcode, so we can isolate
/// the performance impact of LLVM i256 sdiv/srem vs ruint builtins.
///
/// Usage:
///   cargo run -p revmc-examples-runner --bin bench_sdiv --release

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Instant,
};

use revm::{
    bytecode::Bytecode,
    context::{BlockEnv, CfgEnv, TxEnv},
    context_interface::{
        result::{EVMError, HaltReason, InvalidTransaction, ResultAndState},
        ContextSetters,
    },
    database::{CacheDB, EmptyDB},
    handler::{EvmTr, FrameResult, Handler, ItemOrResult, MainBuilder},
    primitives::{hardfork::SpecId, Address, Bytes, TxKind, B256, U256},
    state::AccountInfo,
    ExecuteEvm, MainnetEvm,
};
use revmc::{EvmCompiler, EvmLlvmBackend, OptimizationLevel};
use revmc_context::{EvmCompilerFn, RawEvmCompilerFn};

// ── Constants ───────────────────────────────────────────────────────────────

const CONTRACT: Address = Address::new([0x10; 20]);
const CALLER: Address = Address::new([0xCA; 20]);

// ── Bytecode generator ──────────────────────────────────────────────────────

/// Build EVM bytecode that loops `iterations` times executing `opcode(a, b)`.
///
/// Stores operands in EVM memory and loads them via MLOAD each iteration,
/// so LLVM cannot constant-fold the operations (MLOAD goes through opaque builtins).
fn make_loop_bytecode(opcode: u8, a: U256, b: U256, iterations: u16) -> Vec<u8> {
    let mut code = Vec::with_capacity(128);

    // Store a at memory[0]
    code.push(0x7F); // PUSH32 a
    code.extend_from_slice(&a.to_be_bytes::<32>());
    code.push(0x60); code.push(0x00); // PUSH1 0
    code.push(0x52); // MSTORE

    // Store b at memory[32]
    code.push(0x7F); // PUSH32 b
    code.extend_from_slice(&b.to_be_bytes::<32>());
    code.push(0x60); code.push(0x20); // PUSH1 32
    code.push(0x52); // MSTORE

    // Push loop counter
    code.push(0x61); // PUSH2 iterations
    code.extend_from_slice(&iterations.to_be_bytes());

    // JUMPDEST (record offset for loop target)
    let loop_start = code.len() as u8;
    code.push(0x5B); // JUMPDEST

    // Load operands from memory (opaque to LLVM optimizer)
    code.push(0x60); code.push(0x00); // PUSH1 0
    code.push(0x51); // MLOAD → a
    code.push(0x60); code.push(0x20); // PUSH1 32
    code.push(0x51); // MLOAD → b

    code.push(opcode); // target operation: OP(a, b)
    // Store result to mem[64] (not mem[0/32] which are inputs).
    // This prevents LLVM from dead-code-eliminating the inline operation
    // (MSTORE is a builtin with side effects), while keeping inputs stable
    // (no convergence to 0). LLVM also can't hoist MLOADs out of the loop
    // because MSTORE is opaque and might alias.
    code.push(0x60); code.push(0x40); // PUSH1 64
    code.push(0x52); // MSTORE → mem[64] = result

    // counter -= 1
    code.push(0x60); code.push(0x01); // PUSH1 1
    code.push(0x90); // SWAP1
    code.push(0x03); // SUB

    // loop if counter > 0
    code.push(0x80); // DUP1
    code.push(0x60); code.push(loop_start); // PUSH1 <loop_start>
    code.push(0x57); // JUMPI

    // cleanup + stop
    code.push(0x50); // POP counter
    code.push(0x00); // STOP

    code
}

// ── JIT Handler (from bench_common) ─────────────────────────────────────────

type BenchEvm<'a> = MainnetEvm<revm::handler::MainnetContext<&'a mut CacheDB<EmptyDB>>>;
type BenchError = EVMError<core::convert::Infallible, InvalidTransaction>;

struct JitHandler {
    functions: Arc<HashMap<B256, RawEvmCompilerFn>>,
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
                if !should_lookup_jit(
                    frame.data.is_create(),
                    frame.interpreter.input.bytecode_address,
                    frame.interpreter.bytecode.is_empty(),
                ) {
                    evm.frame_run()?
                } else {
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

// ── Fixture ─────────────────────────────────────────────────────────────────

struct Fixture {
    db: Arc<CacheDB<EmptyDB>>,
    block: BlockEnv,
    cfg: CfgEnv,
    tx: TxEnv,
    functions: Arc<HashMap<B256, RawEvmCompilerFn>>,
    // Leak to keep JIT code alive
    _compiler: &'static mut EvmCompiler<EvmLlvmBackend<'static>>,
    _context: &'static revmc::llvm::inkwell::context::Context,
}

impl Fixture {
    fn build(bytecode: &[u8]) -> Self {
        let code = Bytecode::new_raw(Bytes::from(bytecode.to_vec()));
        let code_hash = code.hash_slow();

        let mut db = CacheDB::new(EmptyDB::new());
        db.insert_account_info(
            CONTRACT,
            AccountInfo {
                balance: U256::ZERO,
                nonce: 1,
                code_hash,
                code: Some(code.clone()),
            },
        );
        db.insert_account_info(
            CALLER,
            AccountInfo {
                balance: U256::from(10u64).pow(U256::from(25)),
                nonce: 0,
                code_hash: revm::primitives::KECCAK_EMPTY,
                code: None,
            },
        );

        // JIT compile
        let context =
            Box::leak(Box::new(revmc::llvm::inkwell::context::Context::create()));
        let backend =
            EvmLlvmBackend::new(context, false, OptimizationLevel::Aggressive).unwrap();
        let compiler: &'static mut EvmCompiler<EvmLlvmBackend<'static>> =
            Box::leak(Box::new(EvmCompiler::new(backend)));

        // Dump assembly to /tmp/jit_dump/ if REVMC_DUMP=1
        if std::env::var("REVMC_DUMP").is_ok() {
            let dump_dir = std::path::PathBuf::from("/tmp/jit_dump");
            std::fs::create_dir_all(&dump_dir).ok();
            compiler.set_dump_to(Some(dump_dir));
        }

        let name = format!("contract_{}", hex::encode(code_hash.as_slice()));
        let func_id = compiler
            .translate(&name, code.original_byte_slice(), SpecId::CANCUN)
            .expect("JIT translation failed");
        let fn_ptr =
            unsafe { compiler.jit_function(func_id).expect("JIT compilation failed") };

        let mut functions = HashMap::new();
        functions.insert(code_hash, fn_ptr.into_inner());
        let functions = Arc::new(functions);

        let cfg = CfgEnv::new_with_spec(SpecId::CANCUN);
        let mut block = BlockEnv::default();
        block.number = U256::from(1);
        block.timestamp = U256::from(1);
        block.gas_limit = 1_000_000_000;
        block.basefee = 1;
        block.set_blob_excess_gas_and_price(
            0,
            revm::primitives::eip4844::BLOB_BASE_FEE_UPDATE_FRACTION_CANCUN,
        );

        let tx = TxEnv {
            tx_type: 0,
            caller: CALLER,
            gas_limit: 1_000_000_000,
            gas_price: 1,
            kind: TxKind::Call(CONTRACT),
            value: U256::ZERO,
            data: Bytes::new(),
            nonce: 0,
            chain_id: Some(cfg.chain_id),
            access_list: Default::default(),
            gas_priority_fee: None,
            blob_hashes: Vec::new(),
            max_fee_per_blob_gas: 0,
            authorization_list: Vec::new(),
        };

        Self {
            db: Arc::new(db),
            block,
            cfg,
            tx,
            functions,
            _compiler: compiler,
            _context: context,
        }
    }

    fn make_evm(&self) -> BenchEvm<'static> {
        let db_ref =
            unsafe { &mut *(Arc::as_ptr(&self.db) as *mut CacheDB<EmptyDB>) };
        let ctx = revm::context::Context::new(db_ref, SpecId::CANCUN);
        let mut evm = ctx.build_mainnet();
        evm.ctx.block = self.block.clone();
        evm.ctx.cfg = self.cfg.clone();
        evm
    }

    fn run_plain(&self) -> ResultAndState {
        let mut evm = self.make_evm();
        evm.transact(self.tx.clone()).expect("plain execution failed")
    }

    fn run_jit(&self) -> ResultAndState {
        let mut evm = self.make_evm();
        evm.ctx.set_tx(self.tx.clone());
        let mut handler = JitHandler { functions: self.functions.clone() };
        let result = handler.run(&mut evm).expect("JIT execution failed");
        let state = evm.ctx.journaled_state.finalize();
        ResultAndState::new(result, state)
    }
}

// ── Benchmark runner ────────────────────────────────────────────────────────

fn bench_opcode(name: &str, opcode: u8, a: U256, b: U256, loop_iters: u16, samples: usize) {
    let bytecode = make_loop_bytecode(opcode, a, b, loop_iters);
    let fixture = Fixture::build(&bytecode);

    // Sanity check
    let r = fixture.run_plain();
    assert!(r.result.is_success(), "{name}: plain reverted");
    let r = fixture.run_jit();
    assert!(r.result.is_success(), "{name}: JIT reverted");

    // Warmup
    for _ in 0..5 {
        let _ = fixture.run_plain();
        let _ = fixture.run_jit();
    }

    // Measure plain
    let mut plain_times = Vec::with_capacity(samples);
    for _ in 0..samples {
        let start = Instant::now();
        let _ = fixture.run_plain();
        plain_times.push(start.elapsed());
    }

    // Measure JIT
    let mut jit_times = Vec::with_capacity(samples);
    for _ in 0..samples {
        let start = Instant::now();
        let _ = fixture.run_jit();
        jit_times.push(start.elapsed());
    }

    plain_times.sort();
    jit_times.sort();
    let plain_med = plain_times[samples / 2];
    let jit_med = jit_times[samples / 2];
    let speedup = plain_med.as_nanos() as f64 / jit_med.as_nanos() as f64;

    println!(
        "  {name:<6}  plain={:>8.1}µs  jit={:>8.1}µs  speedup={:.2}x",
        plain_med.as_nanos() as f64 / 1000.0,
        jit_med.as_nanos() as f64 / 1000.0,
        speedup,
    );
}

// ── Main ────────────────────────────────────────────────────────────────────

fn main() {
    let loop_iters = 10_000u16;
    let samples = 50;

    // Large negative numerator (multi-limb, MSB set → negative in signed i256)
    let a_signed = U256::from_str_radix(
        "F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0",
        16,
    )
    .unwrap();

    // Medium-large positive divisor (multi-limb)
    let b_divisor = U256::from_str_radix(
        "0000000000000000000000000000000100000000000000000000000000000007",
        16,
    )
    .unwrap();

    // Large unsigned numerator (MSB clear → positive in both signed and unsigned)
    let a_unsigned = U256::from_str_radix(
        "70F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0",
        16,
    )
    .unwrap();

    println!("SDIV/SMOD micro-benchmark ({loop_iters} ops/call, {samples} samples)\n");

    println!("Unsigned (DIV/MOD → ruint builtins):");
    bench_opcode("DIV", 0x04, a_unsigned, b_divisor, loop_iters, samples);
    bench_opcode("MOD", 0x06, a_unsigned, b_divisor, loop_iters, samples);

    println!("\nSigned (SDIV/SMOD → inline LLVM i256):");
    bench_opcode("SDIV", 0x05, a_signed, b_divisor, loop_iters, samples);
    bench_opcode("SMOD", 0x07, a_signed, b_divisor, loop_iters, samples);

    println!("\nIf SDIV/SMOD speedup << DIV/MOD speedup → LLVM i256 sdiv/srem is the bottleneck.");
    println!("Fix: wire SDIV/SMOD to existing ruint builtins (already registered, not yet wired).");
}
