//! For a set of code hashes: in how many block snapshots does each appear?
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
    let a: Vec<String> = std::env::args().collect();
    let (dir, start, count) = (&a[1], a[2].parse::<u64>().unwrap(), a[3].parse::<u64>().unwrap());
    let wanted: HashSet<B256> = std::fs::read_to_string(&a[4]).unwrap().lines()
        .filter(|l| !l.trim().is_empty()).map(|l| l.trim().parse().unwrap()).collect();
    let mut freq: HashMap<B256, u32> = HashMap::new();
    for bn in start..start + count {
        let Ok(l) = BinLoader::new(Path::new(dir), bn) else { continue };
        for h in l.snapshot.codes.keys() {
            if wanted.contains(h) { *freq.entry(*h).or_default() += 1; }
        }
    }
    let mut top: Vec<(B256, u32)> = freq.iter().map(|(h, c)| (*h, *c)).collect();
    top.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
    for (h, c) in top.iter().take(8) { println!("TOP 0x{} {c}", hex::encode(h)); }
    let mut v: Vec<u32> = wanted.iter().map(|h| freq.get(h).copied().unwrap_or(0)).collect();
    v.sort_unstable();
    let n = v.len();
    println!("hashes {n} | appears in blocks: min {} p25 {} median {} p75 {} max {}",
        v[0], v[n/4], v[n/2], v[3*n/4], v[n-1]);
    println!("appearing in only 1 block: {}", v.iter().filter(|&&x| x == 1).count());
    println!("appearing in >100 blocks: {}", v.iter().filter(|&&x| x > 100).count());
}
