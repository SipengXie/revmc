//! Find contract addresses for a code_hash, or find tx_index for a target address.
//!
//! Usage:
//!   find_address <code_hash_hex>
//!   find_address --find-tx <address_hex> [--block <block>] [--selector <sel_hex>]
//!   find_address --find-calldata <address_hex> [--block <block>]

#[path = "../bin_common.rs"]
mod bin_common;

use bin_common::{BlockBin, CacheSnapshot};
use revm::primitives::{Address, B256};
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--find-calldata") {
        find_calldata(&args);
    } else if args.iter().any(|a| a == "--find-tx") {
        find_tx(&args);
    } else {
        find_code_hash(&args);
    }
}

/// Find txs whose calldata contains the given address bytes
fn find_calldata(args: &[String]) {
    let idx = args.iter().position(|a| a == "--find-calldata").unwrap() + 1;
    let addr_hex = args.get(idx).expect("--find-calldata requires address");
    let addr_hex = addr_hex.strip_prefix("0x").unwrap_or(addr_hex);
    let addr_bytes = hex::decode(addr_hex).expect("Invalid address hex");

    let block_filter: Option<u64> = args.iter().position(|a| a == "--block").map(|i| {
        args[i + 1].parse().expect("Invalid block number")
    });

    println!("Searching for txs with address in calldata: 0x{}", addr_hex);

    let txs_dir = PathBuf::from("/home/ubuntu/sipeng/bench_data/txs");
    let mut entries: Vec<_> = std::fs::read_dir(&txs_dir)
        .expect("cannot read txs dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map_or(false, |ext| ext == "bin"))
        .collect();
    entries.sort_by_key(|e| e.file_name());

    let mut total_found = 0;
    let max_results = 20;

    for entry in &entries {
        let path = entry.path();
        let block_str = path.file_stem().unwrap().to_str().unwrap().to_string();
        let block_num: u64 = match block_str.parse() {
            Ok(n) => n,
            Err(_) => continue,
        };

        if let Some(bf) = block_filter {
            if block_num != bf { continue; }
        }

        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let block_bin: BlockBin = match bincode::deserialize(&data) {
            Ok(b) => b,
            Err(_) => continue,
        };

        for (tx_idx, tx) in block_bin.txs.iter().enumerate() {
            if tx.tx_type == 0x7e { continue; } // skip deposit
            if tx.data.windows(addr_bytes.len()).any(|w| w == addr_bytes.as_slice()) {
                let selector = if tx.data.len() >= 4 {
                    format!("0x{}", hex::encode(&tx.data[..4]))
                } else {
                    "none".to_string()
                };
                let to_hex = tx.to.map(|t| format!("0x{}", hex::encode(t))).unwrap_or("CREATE".into());
                println!("  block={} tx_index={} to={} gas_limit={} selector={} data_len={}",
                    block_num, tx_idx, to_hex, tx.gas_limit, selector, tx.data.len());
                total_found += 1;
                if total_found >= max_results {
                    println!("  ... (showing first {} results)", max_results);
                    return;
                }
            }
        }
    }

    println!("Total found: {}", total_found);
}

fn find_tx(args: &[String]) {
    let addr_idx = args.iter().position(|a| a == "--find-tx").unwrap() + 1;
    let addr_hex = args.get(addr_idx).expect("--find-tx requires address");
    let addr_hex = addr_hex.strip_prefix("0x").unwrap_or(addr_hex);
    let target_addr: Address = addr_hex.parse().expect("Invalid address");

    let sel_filter: Option<[u8; 4]> = args.iter().position(|a| a == "--selector").map(|i| {
        let s = args[i + 1].strip_prefix("0x").unwrap_or(&args[i + 1]);
        let bytes = hex::decode(s).expect("Invalid selector hex");
        let mut arr = [0u8; 4];
        arr.copy_from_slice(&bytes);
        arr
    });

    let block_filter: Option<u64> = args.iter().position(|a| a == "--block").map(|i| {
        args[i + 1].parse().expect("Invalid block number")
    });

    println!("Searching for txs to address: 0x{}", hex::encode(target_addr));
    if let Some(sel) = &sel_filter {
        println!("  Selector filter: 0x{}", hex::encode(sel));
    }

    let txs_dir = PathBuf::from("/home/ubuntu/sipeng/bench_data/txs");
    let mut entries: Vec<_> = std::fs::read_dir(&txs_dir)
        .expect("cannot read txs dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map_or(false, |ext| ext == "bin"))
        .collect();
    entries.sort_by_key(|e| e.file_name());

    let mut total_found = 0;
    let max_results = 20;

    for entry in &entries {
        let path = entry.path();
        let block_str = path.file_stem().unwrap().to_str().unwrap().to_string();
        let block_num: u64 = match block_str.parse() {
            Ok(n) => n,
            Err(_) => continue,
        };

        if let Some(bf) = block_filter {
            if block_num != bf { continue; }
        }

        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let block_bin: BlockBin = match bincode::deserialize(&data) {
            Ok(b) => b,
            Err(_) => continue,
        };

        for (tx_idx, tx) in block_bin.txs.iter().enumerate() {
            if let Some(to) = &tx.to {
                let to_addr = Address::from_slice(to);
                if to_addr == target_addr {
                    let selector = if tx.data.len() >= 4 {
                        format!("0x{}", hex::encode(&tx.data[..4]))
                    } else {
                        "none".to_string()
                    };

                    if let Some(ref sf) = sel_filter {
                        if tx.data.len() < 4 || &tx.data[..4] != sf {
                            continue;
                        }
                    }

                    println!("  block={} tx_index={} gas_limit={} selector={} data_len={}",
                        block_num, tx_idx, tx.gas_limit, selector, tx.data.len());
                    total_found += 1;

                    if total_found >= max_results {
                        println!("  ... (showing first {} results)", max_results);
                        return;
                    }
                }
            }
        }
    }

    println!("Total found: {}", total_found);
}

fn find_code_hash(args: &[String]) {
    let target_hex = args.get(1).expect("Usage: find_address <code_hash_hex>");
    let target_hex = target_hex.strip_prefix("0x").unwrap_or(target_hex);
    let target: B256 = target_hex.parse().expect("Invalid code_hash hex");

    println!("Searching for code_hash: {:#x}", target);

    let states_dir = PathBuf::from("/home/ubuntu/sipeng/bench_data/states");
    let mut entries: Vec<_> = std::fs::read_dir(&states_dir)
        .expect("cannot read states dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map_or(false, |ext| ext == "bin"))
        .collect();
    entries.sort_by_key(|e| e.file_name());

    let mut found = false;
    if let Some(entry) = entries.first() {
        let path = entry.path();
        let block = path.file_stem().unwrap().to_str().unwrap().to_string();
        println!("Loading state from block {}...", block);
        let data = std::fs::read(&path).unwrap();
        let snapshot: CacheSnapshot = bincode::deserialize(&data).unwrap();
        println!("  {} accounts in snapshot", snapshot.accounts.len());

        for (addr, acct) in &snapshot.accounts {
            if let Some(ref info) = acct.info {
                if info.code_hash == target {
                    println!("  => Address: 0x{}", hex::encode(addr.as_slice()));
                    found = true;
                }
            }
        }
    }

    if !found {
        println!("Code hash not found in any state snapshot.");
    }
    println!("Done.");
}
