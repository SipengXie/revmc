//! Inspect bytecodes from a block snapshot to identify contract patterns.
//!
//! Usage:
//!   cargo run -p revmc-examples-runner --bin inspect_contracts --release -- --block 38004930

#[allow(dead_code)]
#[path = "../bin_common.rs"]
mod bin_common;

use std::path::Path;
use clap::Parser;
use revm::primitives::Address;
use bin_common::BinLoader;

// Target hashes from frame_bench analysis (first 6 hex chars = 3 bytes)
const TARGETS: &[(&str, &str)] = &[
    // JIT-unfriendly 45B proxies
    ("7dd6ff", "45B proxy calls=42 0.86x"),
    ("183082", "45B proxy calls=28 0.90x"),
    ("072cfd", "45B proxy calls=16 0.92x"),
    ("6667ce", "45B proxy calls=8  0.81x"),
    ("acd671", "45B proxy calls=251 1.09x"),
    // JIT-unfriendly large contracts
    ("67c83f", "22142B calls=20 0.85x"),
    ("e07d48", "22142B calls=8  0.80x"),
    ("ffe162", "22493B calls=10 0.86x"),
    ("71e220", "22962B calls=8  0.84x"),
    ("91a0e9", "22142B calls=20 0.97x"),
    ("465dc5", "16912B calls=7  0.79x"),
    // JIT-friendly top contracts (for comparison)
    ("83b2af", "24009B calls=286 1.95x GOOD"),
    ("19e076", "4774B  calls=30  2.73x GOOD"),
    ("be49ac", "4270B  calls=18  2.56x GOOD"),
    ("772fb5", "24279B calls=251 1.81x GOOD"),
    ("11b75a", "23464B calls=159 1.25x GOOD"),
];

// Known EVM bytecode patterns
fn identify_pattern(bytes: &[u8]) -> &'static str {
    if bytes.is_empty() { return "empty"; }
    // EIP-1167 minimal proxy: 363d3d373d3d3d363d73...5af43d82803e903d91602b57fd5bf3
    if bytes.len() == 45 && bytes.starts_with(&[0x36, 0x3d, 0x3d, 0x37]) {
        return "EIP-1167 minimal proxy";
    }
    // EIP-1167 variant starting with 3d (some deployments)
    if bytes.len() <= 50 && (bytes.starts_with(&[0x36, 0x3d]) || bytes.starts_with(&[0x3d, 0x3d])) {
        return "minimal proxy variant";
    }
    // ERC20 proxy check: many use DELEGATECALL (0xf4)
    if bytes.contains(&0xf4) && bytes.len() < 200 {
        return "small delegatecall proxy";
    }
    // Check for SLOAD density in first 64 bytes
    let sload_count = bytes[..bytes.len().min(256)].iter().filter(|&&b| b == 0x54).count();
    let sstore_count = bytes[..bytes.len().min(256)].iter().filter(|&&b| b == 0x55).count();
    let total = bytes.len().min(256);
    if sload_count + sstore_count > total / 10 {
        return "storage-heavy (high SLOAD/SSTORE density)";
    }
    "general contract"
}

fn storage_fraction(bytes: &[u8]) -> f64 {
    let mut total = 0u32;
    let mut storage = 0u32;
    let mut i = 0;
    while i < bytes.len() {
        let op = bytes[i];
        total += 1;
        if op == 0x54 || op == 0x55 { storage += 1; }
        if op >= 0x60 && op <= 0x7f { i += (op - 0x5f) as usize; } else { i += 1; }
    }
    if total == 0 { 0.0 } else { storage as f64 / total as f64 }
}

fn call_fraction(bytes: &[u8]) -> f64 {
    let mut total = 0u32;
    let mut calls = 0u32;
    let mut i = 0;
    while i < bytes.len() {
        let op = bytes[i];
        total += 1;
        // CALL=0xf1, CALLCODE=0xf2, DELEGATECALL=0xf4, STATICCALL=0xfa
        if matches!(op, 0xf1 | 0xf2 | 0xf4 | 0xfa) { calls += 1; }
        if op >= 0x60 && op <= 0x7f { i += (op - 0x5f) as usize; } else { i += 1; }
    }
    if total == 0 { 0.0 } else { calls as f64 / total as f64 }
}

#[derive(Parser)]
#[command(name = "inspect_contracts")]
struct Args {
    #[arg(long, default_value = "/home/ubuntu/sipeng/bench_data")]
    dir: String,
    #[arg(long, default_value_t = 38004930)]
    block: u64,
}

/// For EIP-1167 proxy: extract the embedded implementation address (bytes 10..30).
fn eip1167_impl_addr(bytes: &[u8]) -> Option<Address> {
    if bytes.len() == 45 && bytes.starts_with(&[0x36, 0x3d, 0x3d, 0x37]) && bytes[9] == 0x73 {
        Some(Address::from_slice(&bytes[10..30]))
    } else {
        None
    }
}

fn main() {
    let args = Args::parse();
    let loader = BinLoader::new(Path::new(&args.dir), args.block)
        .expect("load block");

    println!("Block {} | {} codes in snapshot\n", args.block, loader.code_count());

    // Also show: how many txs target each proxy address
    let tx_targets: Vec<Option<Address>> = loader.raw_txs().iter()
        .map(|tx| tx.to.map(|a| Address::from_slice(&a)))
        .collect();

    for &(prefix, label) in TARGETS {
        let found = loader.code_values().iter().find(|(hash, _)| {
            hex::encode(hash.as_slice()).starts_with(prefix)
        });

        match found {
            None => println!("  [{prefix}..] NOT FOUND ({label})"),
            Some((hash, bytecode)) => {
                let bytes = bytecode.original_byte_slice();
                let pattern = identify_pattern(bytes);
                let sf = storage_fraction(bytes);
                let cf = call_fraction(bytes);
                println!("  [{prefix}..] {label}");
                println!("    hash:    {}", hex::encode(hash.as_slice()));
                println!("    size:    {}B  pattern: {}  storage={:.3}  calls={:.3}",
                    bytes.len(), pattern, sf, cf);

                // Find addresses with this code
                let addrs: Vec<Address> = loader.snapshot.accounts.iter()
                    .filter(|(_, acc)| acc.info.as_ref().map(|i| i.code_hash == *hash).unwrap_or(false))
                    .map(|(addr, _)| *addr)
                    .collect();

                // Count direct txs to these addresses
                let direct_tx_count: usize = tx_targets.iter()
                    .filter(|t| t.map(|a| addrs.contains(&a)).unwrap_or(false))
                    .count();
                println!("    addresses: {}  direct_txs: {}", addrs.len(), direct_tx_count);
                for addr in addrs.iter().take(2) {
                    println!("      {addr:?}");
                }

                // For EIP-1167 proxies: show implementation
                if let Some(impl_addr) = eip1167_impl_addr(bytes) {
                    println!("    impl_addr: {impl_addr:?}");
                    // Look up impl in snapshot
                    if let Some(impl_acc) = loader.snapshot.accounts.get(&impl_addr) {
                        if let Some(info) = &impl_acc.info {
                            let impl_hash = info.code_hash;
                            if let Some(impl_bc) = loader.code_values().get(&impl_hash) {
                                let ib = impl_bc.original_byte_slice();
                                println!("    impl:      {}B  storage={:.3}  pattern={}",
                                    ib.len(), storage_fraction(ib), identify_pattern(ib));
                                println!("    impl_hash: {}", &hex::encode(impl_hash.as_slice())[..16]);
                                let n_storage = impl_acc.storage.len();
                                println!("    impl_storage_slots: {n_storage}");
                            }
                        }
                    } else {
                        println!("    impl: NOT in snapshot");
                    }
                }

                // For large contracts: show tx calldata selectors
                if bytes.len() > 1000 {
                    let mut selectors: std::collections::HashMap<[u8; 4], usize> = Default::default();
                    for tx in loader.raw_txs() {
                        if let Some(to) = tx.to {
                            let to_addr = Address::from_slice(&to);
                            if addrs.contains(&to_addr) && tx.data.len() >= 4 {
                                let sel: [u8; 4] = tx.data[..4].try_into().unwrap();
                                *selectors.entry(sel).or_default() += 1;
                            }
                        }
                    }
                    if !selectors.is_empty() {
                        let mut sel_vec: Vec<_> = selectors.into_iter().collect();
                        sel_vec.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
                        print!("    direct_selectors: ");
                        for (sel, count) in sel_vec.iter().take(3) {
                            print!("{}({count}) ", hex::encode(sel));
                        }
                        println!();
                    }
                }
                println!();
            }
        }
    }
}
