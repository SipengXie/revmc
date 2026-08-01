//! Reverse lookup: code_hash -> addresses holding it, over a block range.
#[path = "../bin_common.rs"]
#[allow(dead_code)]
mod bin_common;
#[path = "../skelot_common.rs"]
#[allow(dead_code)]
mod skelot_common;
use bin_common::BinLoader;
use revm::primitives::B256;
use std::collections::{HashMap, HashSet};
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let bench_dir = &args[1];
    let start: u64 = args[2].parse().unwrap();
    let count: u64 = args[3].parse().unwrap();
    let wanted: HashSet<B256> = std::fs::read_to_string(&args[4])
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().parse().unwrap())
        .collect();
    let mut found: HashMap<B256, String> = HashMap::new();
    for bn in start..start + count {
        let Ok(loader) = BinLoader::new(Path::new(bench_dir), bn) else { continue };
        for (addr, acc) in &loader.snapshot.accounts {
            if let Some(info) = &acc.info {
                if wanted.contains(&info.code_hash) && !found.contains_key(&info.code_hash) {
                    found.insert(info.code_hash, format!("0x{}", hex::encode(addr.as_slice())));
                }
            }
        }
        if found.len() == wanted.len() { break; }
    }
    for (h, a) in &found {
        println!("0x{} {}", hex::encode(h), a);
    }
    eprintln!("resolved {}/{}", found.len(), wanted.len());
}
