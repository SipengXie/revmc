//! Audit one snapshot: how many `codes` entries are referenced by an account?
#[path = "../bin_common.rs"]
#[allow(dead_code)]
mod bin_common;
#[path = "../skelot_common.rs"]
#[allow(dead_code)]
mod skelot_common;
use bin_common::BinLoader;
use revm::primitives::B256;
use std::collections::HashSet;
use std::path::Path;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let dir = &a[1];
    let start: u64 = a[2].parse().unwrap();
    let count: u64 = a[3].parse().unwrap();
    let (mut tc, mut tr, mut ta, mut nb) = (0usize, 0usize, 0usize, 0usize);
    for bn in start..start + count {
        let Ok(l) = BinLoader::new(Path::new(dir), bn) else { continue };
        let referenced: HashSet<B256> = l
            .snapshot
            .accounts
            .values()
            .filter_map(|acc| acc.info.as_ref().map(|i| i.code_hash))
            .collect();
        let codes: HashSet<B256> = l.snapshot.codes.keys().copied().collect();
        tc += codes.len();
        tr += codes.intersection(&referenced).count();
        ta += l.snapshot.accounts.len();
        nb += 1;
    }
    println!("blocks {nb} | accounts {ta} | codes entries {tc} | of which referenced by an account: {tr} ({:.1}%)",
        tr as f64 / tc as f64 * 100.0);
}
