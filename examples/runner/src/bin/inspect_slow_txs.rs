//! Inspect the slowest JIT txs from bench_proxy --all-txs analysis.
//!
//! For each target tx index, shows:
//!   - tx metadata (type, caller, to, gas_limit, calldata selector)
//!   - contract bytecode analysis (size, storage/call opcode fractions, unique opcodes)
//!   - for CREATE: init code analysis
//!
//! Usage:
//!   cargo run -p revmc-examples-runner --bin inspect_slow_txs --release

#[allow(dead_code)]
#[path = "../bin_common.rs"]
mod bin_common;

use std::path::Path;
use std::collections::{HashMap, HashSet};

use clap::Parser;
use revm::primitives::Address;
use bin_common::BinLoader;

// Worst JIT txs from bench_proxy --all-txs (sorted by speedup ascending)
const SLOW_TX_INDICES: &[usize] = &[144, 128, 175, 10, 130, 208, 4, 82, 116, 121];

// ── Bytecode analysis ────────────────────────────────────────────────────────

struct OpcodeStats {
    total_ops: u32,
    storage_ops: u32,  // SLOAD + SSTORE
    call_ops: u32,     // CALL CALLCODE DELEGATECALL STATICCALL
    jump_ops: u32,     // JUMP JUMPI
    push_ops: u32,
    unique_opcodes: HashSet<u8>,
}

fn analyze_bytecode(bytes: &[u8]) -> OpcodeStats {
    let mut st = OpcodeStats {
        total_ops: 0,
        storage_ops: 0,
        call_ops: 0,
        jump_ops: 0,
        push_ops: 0,
        unique_opcodes: HashSet::new(),
    };
    let mut i = 0;
    while i < bytes.len() {
        let op = bytes[i];
        st.total_ops += 1;
        st.unique_opcodes.insert(op);
        match op {
            0x54 | 0x55 => st.storage_ops += 1,
            0xf1 | 0xf2 | 0xf4 | 0xfa => st.call_ops += 1,
            0x56 | 0x57 => st.jump_ops += 1,
            0x60..=0x7f => {
                st.push_ops += 1;
                i += (op - 0x5f) as usize;
            }
            _ => {}
        }
        i += 1;
    }
    st
}

fn identify_pattern(bytes: &[u8]) -> &'static str {
    if bytes.is_empty() { return "empty"; }
    if bytes.len() == 45 && bytes.starts_with(&[0x36, 0x3d, 0x3d, 0x37]) {
        return "EIP-1167 minimal proxy";
    }
    if bytes.len() <= 50 && (bytes.starts_with(&[0x36, 0x3d]) || bytes.starts_with(&[0x3d, 0x3d])) {
        return "minimal proxy variant";
    }
    let st = analyze_bytecode(bytes);
    if st.total_ops > 0 {
        let sf = st.storage_ops as f64 / st.total_ops as f64;
        let cf = st.call_ops as f64 / st.total_ops as f64;
        if sf > 0.15 { return "storage-heavy"; }
        if cf > 0.05 { return "call-heavy"; }
    }
    "general"
}

// ── CLI ──────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "inspect_slow_txs")]
struct Args {
    #[arg(long, default_value = "/home/ubuntu/sipeng/bench_data")]
    dir: String,
    #[arg(long, default_value_t = 38004930)]
    block: u64,
}

fn main() {
    let args = Args::parse();
    let loader = BinLoader::new(Path::new(&args.dir), args.block).expect("load block");

    // Build address → code hash map from snapshot
    let addr_to_hash: HashMap<Address, _> = loader.snapshot.accounts.iter()
        .filter_map(|(addr, acc)| {
            acc.info.as_ref().and_then(|i| {
                // skip zero code hash (EOA)
                if i.code_hash == revm::primitives::KECCAK_EMPTY { None }
                else { Some((*addr, i.code_hash)) }
            })
        })
        .collect();

    println!("=== Inspect Slow JIT Txs: Block {} ===\n", args.block);

    for &idx in SLOW_TX_INDICES {
        let tx = &loader.raw_txs()[idx];
        let to_addr = tx.to.map(|a| Address::from_slice(&a));
        let caller = Address::from_slice(&tx.caller);
        let selector = if tx.data.len() >= 4 {
            format!("{}", hex::encode(&tx.data[..4]))
        } else if tx.data.is_empty() {
            "(empty calldata)".to_string()
        } else {
            format!("partial:{}", hex::encode(&tx.data))
        };

        println!("── tx[{idx}] ──────────────────────────────────────────────");
        println!("  type:     0x{:02x}", tx.tx_type);
        println!("  caller:   {:?}", caller);
        println!("  gas_limit:{}", tx.gas_limit);
        println!("  calldata: {}B  selector={}", tx.data.len(), selector);

        match to_addr {
            None => {
                // CREATE tx — analyze init code
                let bytes = &tx.data;
                println!("  kind:     CREATE");
                println!("  initcode: {}B", bytes.len());
                if !bytes.is_empty() {
                    let st = analyze_bytecode(bytes);
                    let sf = if st.total_ops > 0 { st.storage_ops as f64 / st.total_ops as f64 } else { 0.0 };
                    let cf = if st.total_ops > 0 { st.call_ops as f64 / st.total_ops as f64 } else { 0.0 };
                    let jf = if st.total_ops > 0 { st.jump_ops as f64 / st.total_ops as f64 } else { 0.0 };
                    println!("  initcode analysis:");
                    println!("    total_ops={} unique_opcodes={}", st.total_ops, st.unique_opcodes.len());
                    println!("    storage={:.3}  calls={:.3}  jumps={:.3}  push={:.3}",
                        sf, cf, jf, st.push_ops as f64 / st.total_ops as f64);
                    println!("    pattern: {}", identify_pattern(bytes));
                }
            }
            Some(addr) => {
                println!("  to:       {:?}", addr);
                match addr_to_hash.get(&addr) {
                    None => println!("  contract: NOT IN SNAPSHOT (EOA or not loaded)"),
                    Some(&hash) => {
                        match loader.code_values().get(&hash) {
                            None => println!("  contract: hash {} — bytecode not in codes map", &hex::encode(hash.as_slice())[..12]),
                            Some(bc) => {
                                let bytes = bc.original_byte_slice();
                                let st = analyze_bytecode(bytes);
                                let sf = if st.total_ops > 0 { st.storage_ops as f64 / st.total_ops as f64 } else { 0.0 };
                                let cf = if st.total_ops > 0 { st.call_ops as f64 / st.total_ops as f64 } else { 0.0 };
                                let jf = if st.total_ops > 0 { st.jump_ops as f64 / st.total_ops as f64 } else { 0.0 };
                                println!("  contract: {} ({}B)", &hex::encode(hash.as_slice())[..12], bytes.len());
                                println!("    pattern: {}", identify_pattern(bytes));
                                println!("    total_ops={} unique_opcodes={}", st.total_ops, st.unique_opcodes.len());
                                println!("    storage={:.3}  calls={:.3}  jumps={:.3}  push={:.3}",
                                    sf, cf, jf, st.push_ops as f64 / st.total_ops as f64);

                                // Check if this is a proxy — show impl
                                if bytes.len() == 45 && bytes.starts_with(&[0x36, 0x3d, 0x3d, 0x37]) && bytes[9] == 0x73 {
                                    let impl_addr = Address::from_slice(&bytes[10..30]);
                                    println!("    → proxy impl: {:?}", impl_addr);
                                    if let Some(&impl_hash) = addr_to_hash.get(&impl_addr) {
                                        if let Some(impl_bc) = loader.code_values().get(&impl_hash) {
                                            let ib = impl_bc.original_byte_slice();
                                            let ist = analyze_bytecode(ib);
                                            let isf = if ist.total_ops > 0 { ist.storage_ops as f64 / ist.total_ops as f64 } else { 0.0 };
                                            println!("    → impl: {} ({}B)  storage={:.3}  pattern={}",
                                                &hex::encode(impl_hash.as_slice())[..12], ib.len(), isf, identify_pattern(ib));
                                        }
                                    }
                                }

                                // Show storage slots used by this account
                                if let Some(acc) = loader.snapshot.accounts.get(&addr) {
                                    println!("    storage_slots_in_snapshot: {}", acc.storage.len());
                                }
                            }
                        }
                    }
                }
            }
        }
        println!();
    }
}
