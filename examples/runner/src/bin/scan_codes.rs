//! Quick scanner: count unique bytecodes across all bench_data blocks.
//! No LLVM / compilation — just deserialization and hash collection.

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use revm::bytecode::Bytecode;
use revm::primitives::{Address, B256, HashMap as RevmHashMap, U256};
use revm::state::AccountInfo;
use serde::Deserialize;

#[derive(Deserialize)]
struct AccountSnapshot {
    #[allow(dead_code)]
    info: Option<AccountInfo>,
    #[allow(dead_code)]
    storage: RevmHashMap<U256, U256>,
}

#[derive(Deserialize)]
struct CacheSnapshot {
    #[allow(dead_code)]
    block_number: u64,
    #[allow(dead_code)]
    has_state_clear: bool,
    #[allow(dead_code)]
    accounts: RevmHashMap<Address, AccountSnapshot>,
    codes: RevmHashMap<B256, Bytecode>,
}

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "/home/ubuntu/sipeng/bench_data".into());
    let bench_dir = Path::new(&dir);
    let start: u64 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(38004930);
    let count: u64 = std::env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(9997);
    let end = start + count;

    println!("Scanning blocks {start}..{end} ({count} blocks)");

    let t0 = Instant::now();
    let mut all_codes: HashMap<B256, usize> = HashMap::new(); // hash -> bytecode len
    let mut scanned = 0u64;
    let mut total_bytecode_bytes: usize = 0;

    for bn in start..end {
        let path = bench_dir.join(format!("states/{bn}.bin"));
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let snapshot: CacheSnapshot = match bincode::deserialize(&data) {
            Ok(s) => s,
            Err(_) => continue,
        };
        for (hash, bytecode) in snapshot.codes {
            all_codes.entry(hash).or_insert_with(|| {
                let len = bytecode.original_byte_slice().len();
                total_bytecode_bytes += len;
                len
            });
        }
        scanned += 1;
        if scanned % 1000 == 0 {
            eprintln!("  {scanned} blocks scanned, {} unique codes", all_codes.len());
        }
    }

    let elapsed = t0.elapsed();
    let non_empty = all_codes.values().filter(|&&len| len > 0).count();

    // Size distribution
    let mut sizes: Vec<usize> = all_codes.values().copied().filter(|&l| l > 0).collect();
    sizes.sort();

    println!("\n=== Results ===");
    println!("Scanned: {scanned} blocks in {:.1}s", elapsed.as_secs_f64());
    println!("Total unique bytecodes: {}", all_codes.len());
    println!("  Non-empty: {non_empty}");
    println!("  Empty: {}", all_codes.len() - non_empty);
    println!("Total bytecode: {:.1} MB", total_bytecode_bytes as f64 / 1_048_576.0);

    if !sizes.is_empty() {
        let p50 = sizes[sizes.len() / 2];
        let p90 = sizes[sizes.len() * 9 / 10];
        let p99 = sizes[sizes.len() * 99 / 100];
        println!("\nSize distribution (non-empty):");
        println!("  min={}, p50={}, p90={}, p99={}, max={}", sizes[0], p50, p90, p99, sizes[sizes.len() - 1]);

        // Buckets
        let tiny = sizes.iter().filter(|&&s| s < 500).count();
        let small = sizes.iter().filter(|&&s| s >= 500 && s < 5000).count();
        let medium = sizes.iter().filter(|&&s| s >= 5000 && s < 15000).count();
        let large = sizes.iter().filter(|&&s| s >= 15000).count();
        println!("  <500B: {tiny} | 500B-5KB: {small} | 5KB-15KB: {medium} | >15KB: {large}");
    }
}
