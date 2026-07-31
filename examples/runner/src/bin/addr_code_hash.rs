//! One-shot: print code_hash for given addresses from a block snapshot.
#[path = "../bin_common.rs"]
#[allow(dead_code)]
mod bin_common;
#[path = "../skelot_common.rs"]
#[allow(dead_code)]
mod skelot_common;

use bin_common::BinLoader;
use revm::primitives::Address;
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let bench_dir = &args[1];
    let block: u64 = args[2].parse().unwrap();
    let loader = BinLoader::new(Path::new(bench_dir), block).expect("load");
    for a in &args[3..] {
        let addr: Address = a.parse().expect("addr");
        match loader.snapshot.accounts.get(&addr).and_then(|acc| acc.info.as_ref()) {
            Some(info) => println!("{a} -> code_hash 0x{}", hex::encode(info.code_hash)),
            None => println!("{a} -> NOT IN SNAPSHOT"),
        }
    }
}
