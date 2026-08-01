//! Why is a contract's code in the replay corpus if it never executes?
//!
//! Instruments every opcode that *reads* an account without running it —
//! BALANCE, EXTCODESIZE, EXTCODEHASH, EXTCODECOPY — plus the call family
//! (CALL/CALLCODE/DELEGATECALL/STATICCALL, which DO execute the target).
//! For each address referenced, the target's code hash is resolved from
//! state and tallied per opcode, split by whether that code hash ever
//! executes anywhere in the window.
//!
//! Untimed diagnostic: never run alongside a timed measurement.

#[path = "../bin_common.rs"]
#[allow(dead_code)]
mod bin_common;

#[path = "../skelot_common.rs"]
#[allow(dead_code)]
mod skelot_common;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use clap::Parser;
use revm::bytecode::opcode;
use revm::context::ContextTr;
use revm::inspector::Inspector;
use revm::interpreter::interpreter_types::Jumps;
use revm::interpreter::{interpreter::EthInterpreter, Interpreter};
use revm::primitives::{Address, B256, U256};

use bin_common::{build_op_tx, BinLoader};

#[derive(Parser)]
#[command(name = "load_reason_audit")]
struct Args {
    #[arg(long, default_value = "/home/ubuntu/sipeng/bench_data_base")]
    bench_dir: String,
    #[arg(long, default_value_t = 38004930)]
    start_block: u64,
    #[arg(long, default_value_t = 200)]
    block_count: u64,
    /// Newline-separated `0x…` code hashes that never execute (from the
    /// replay's per_target set complement).
    #[arg(long)]
    never_executed: String,
}

#[derive(Default)]
struct Audit {
    /// (opcode name) -> count, for references to never-executed code
    never: HashMap<&'static str, u64>,
    /// same, for references to code that does execute somewhere
    executed: HashMap<&'static str, u64>,
    /// addresses referenced by a read-only opcode, for reporting
    never_addrs: HashSet<Address>,
}

fn op_name(op: u8) -> Option<&'static str> {
    Some(match op {
        opcode::BALANCE => "BALANCE",
        opcode::EXTCODESIZE => "EXTCODESIZE",
        opcode::EXTCODEHASH => "EXTCODEHASH",
        opcode::EXTCODECOPY => "EXTCODECOPY",
        opcode::CALL => "CALL",
        opcode::CALLCODE => "CALLCODE",
        opcode::DELEGATECALL => "DELEGATECALL",
        opcode::STATICCALL => "STATICCALL",
        opcode::SELFDESTRUCT => "SELFDESTRUCT",
        _ => return None,
    })
}

struct AuditInspector {
    audit: Audit,
    never_hashes: std::sync::Arc<HashSet<B256>>,
    code_of: HashMap<Address, B256>,
}

impl<CTX> Inspector<CTX, EthInterpreter> for AuditInspector
where
    CTX: ContextTr,
{
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _ctx: &mut CTX) {
        let op = interp.bytecode.opcode();
        let Some(name) = op_name(op) else { return };
        // The address operand sits on top of the stack for every opcode
        // above (for the call family it is the second word: gas, addr).
        let depth = if matches!(op, opcode::CALL | opcode::CALLCODE | opcode::DELEGATECALL | opcode::STATICCALL) {
            1
        } else {
            0
        };
        // Read-only peek: `data()` is the raw stack vector, top is last.
        let data = interp.stack.data();
        if data.len() <= depth {
            return;
        }
        let word = data[data.len() - 1 - depth];
        let addr = Address::from_word(word.into());
        let Some(hash) = self.code_of.get(&addr) else { return };
        if self.never_hashes.contains(hash) {
            *self.audit.never.entry(name).or_default() += 1;
            self.audit.never_addrs.insert(addr);
        } else {
            *self.audit.executed.entry(name).or_default() += 1;
        }
    }
}

fn main() {
    let args = Args::parse();
    let never_hashes: HashSet<B256> = std::fs::read_to_string(&args.never_executed)
        .expect("read never-executed list")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().parse().expect("hash"))
        .collect();
    eprintln!("never-executed hashes: {}", never_hashes.len());

    let never_hashes = std::sync::Arc::new(never_hashes);
    let mut audit = Audit::default();
    let bench_dir = Path::new(&args.bench_dir);
    let mut blocks = 0u64;

    for bn in args.start_block..args.start_block + args.block_count {
        let Ok(loader) = BinLoader::new(bench_dir, bn) else { continue };
        // address -> code hash, from this block's pre-state
        let code_of: HashMap<Address, B256> = loader
            .snapshot
            .accounts
            .iter()
            .filter_map(|(a, acc)| acc.info.as_ref().map(|i| (*a, i.code_hash)))
            .collect();
        let chain_id = loader.raw_txs().first().and_then(|t| t.chain_id);

        // One EVM per block so intra-block state carries across txs, as in
        // the normal replay path.
        let insp = AuditInspector {
            audit: Audit::default(),
            never_hashes: never_hashes.clone(),
            code_of,
        };
        let mut evm = bin_common::make_evm_with_inspector(&loader, chain_id, insp);
        for tx in loader.raw_txs() {
            if tx.tx_type == 0x7e {
                continue;
            }
            let op_tx = build_op_tx(tx);
            let _ = <_ as revm::inspector::InspectEvm>::inspect_one_tx(&mut evm, op_tx);
        }
        let done = evm.0.inspector;
        for (k, v) in done.audit.never { *audit.never.entry(k).or_default() += v; }
        for (k, v) in done.audit.executed { *audit.executed.entry(k).or_default() += v; }
        audit.never_addrs.extend(done.audit.never_addrs);
        blocks += 1;
        if blocks % 50 == 0 {
            eprintln!("  [{blocks}] blocks");
        }
    }

    let show = |label: &str, m: &HashMap<&'static str, u64>| {
        let total: u64 = m.values().sum();
        let mut v: Vec<_> = m.iter().collect();
        v.sort_by_key(|(_, c)| std::cmp::Reverse(**c));
        println!("\n{label} (total {total}):");
        for (k, c) in v {
            println!("  {k:<14} {c:>10}  ({:5.1}%)", *c as f64 / total.max(1) as f64 * 100.0);
        }
    };
    println!("blocks audited: {blocks}");
    show("references to NEVER-EXECUTED code", &audit.never);
    show("references to code that DOES execute", &audit.executed);
    println!("\ndistinct never-executed addresses referenced: {}", audit.never_addrs.len());
    let _ = U256::ZERO;
}
