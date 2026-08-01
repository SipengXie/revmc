//! Stage-E tx index: `code_hash -> (earliest block, tx_index)` over a block
//! range. Split out of `bin_common.rs`, which re-exports everything here.

use std::collections::HashMap;
use std::path::Path;

use revm::primitives::{Address, B256};

use super::{BlockBin, CacheSnapshot};

/// Build `code_hash → (earliest block, tx_index)` over a block range, resolving
/// `tx.to` addresses to code hashes via each block's `CacheSnapshot.accounts`.
/// Streams blocks in order, writing only the FIRST occurrence for each wanted
/// hash, and short-circuits when every wanted hash has been resolved.
pub fn build_tx_index(
    bench_dir: &Path,
    start_block: u64,
    block_count: u64,
    wanted: &std::collections::HashSet<B256>,
) -> HashMap<B256, (u64, usize)> {
    let mut index: HashMap<B256, (u64, usize)> = HashMap::new();
    for bn in start_block..start_block + block_count {
        if index.len() == wanted.len() {
            break;
        }
        let states_path = bench_dir.join(format!("states/{bn}.bin"));
        let txs_path = bench_dir.join(format!("txs/{bn}.bin"));
        let Ok(sdata) = std::fs::read(&states_path) else { continue };
        let Ok(snapshot): Result<CacheSnapshot, _> = bincode::deserialize(&sdata) else {
            continue;
        };
        let Ok(tdata) = std::fs::read(&txs_path) else { continue };
        let Ok(block_bin): Result<BlockBin, _> = bincode::deserialize(&tdata) else { continue };
        let mut addr_to_hash: HashMap<Address, B256> = HashMap::new();
        for (addr, acc) in &snapshot.accounts {
            if let Some(info) = &acc.info {
                if wanted.contains(&info.code_hash) {
                    addr_to_hash.insert(*addr, info.code_hash);
                }
            }
        }
        for (tx_idx, tx) in block_bin.txs.iter().enumerate() {
            let Some(to) = tx.to else { continue };
            let to_addr = Address::from_slice(&to);
            if let Some(hash) = addr_to_hash.get(&to_addr) {
                index.entry(*hash).or_insert((bn, tx_idx));
            }
        }
    }
    index
}

/// Unfiltered variant of `build_tx_index` that resolves every code hash seen
/// in the block range. Output is a superset of what `build_tx_index(..wanted)`
/// would return, restricted to hashes that appear as `tx.to` recipients.
fn build_tx_index_full(
    bench_dir: &Path,
    start_block: u64,
    block_count: u64,
) -> HashMap<B256, (u64, usize)> {
    let mut index: HashMap<B256, (u64, usize)> = HashMap::new();
    for bn in start_block..start_block + block_count {
        let states_path = bench_dir.join(format!("states/{bn}.bin"));
        let txs_path = bench_dir.join(format!("txs/{bn}.bin"));
        let Ok(sdata) = std::fs::read(&states_path) else { continue };
        let Ok(snapshot): Result<CacheSnapshot, _> = bincode::deserialize(&sdata) else {
            continue;
        };
        let Ok(tdata) = std::fs::read(&txs_path) else { continue };
        let Ok(block_bin): Result<BlockBin, _> = bincode::deserialize(&tdata) else { continue };
        let mut addr_to_hash: HashMap<Address, B256> = HashMap::new();
        for (addr, acc) in &snapshot.accounts {
            if let Some(info) = &acc.info {
                addr_to_hash.insert(*addr, info.code_hash);
            }
        }
        for (tx_idx, tx) in block_bin.txs.iter().enumerate() {
            let Some(to) = tx.to else { continue };
            let to_addr = Address::from_slice(&to);
            if let Some(hash) = addr_to_hash.get(&to_addr) {
                index.entry(*hash).or_insert((bn, tx_idx));
            }
        }
    }
    index
}

/// Cached wrapper around `build_tx_index_full`. Reads from
/// `~/.cache/skelot_index/tx_index_{corpus}_{start_block}_{block_count}.bin`
/// when present, otherwise rebuilds and atomically writes the cache. The corpus
/// under `bench_dir` is assumed static; remove the cache file if it changes.
pub fn build_tx_index_cached(
    bench_dir: &Path,
    start_block: u64,
    block_count: u64,
) -> HashMap<B256, (u64, usize)> {
    let cache_dir = std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".cache").join("skelot_index"))
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp/skelot_index"));
    // Key includes the corpus directory name: two chains (or corpus versions)
    // over the same block window index different transactions.
    let corpus_tag = bench_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "corpus".into());
    let cache_path =
        cache_dir.join(format!("tx_index_{corpus_tag}_{start_block}_{block_count}.bin"));
    if let Ok(data) = std::fs::read(&cache_path) {
        match bincode::deserialize::<HashMap<B256, (u64, usize)>>(&data) {
            Ok(map) => {
                eprintln!(
                    "[cache] loaded tx_index ({} entries) from {}",
                    map.len(),
                    cache_path.display()
                );
                return map;
            }
            Err(e) => eprintln!("[cache] {} unreadable ({e}); rebuilding", cache_path.display()),
        }
    }
    let map = build_tx_index_full(bench_dir, start_block, block_count);
    let _ = std::fs::create_dir_all(&cache_dir);
    match bincode::serialize(&map) {
        Ok(bytes) => {
            let tmp = cache_path.with_extension("bin.tmp");
            if std::fs::write(&tmp, &bytes).is_ok() && std::fs::rename(&tmp, &cache_path).is_ok() {
                eprintln!(
                    "[cache] wrote tx_index ({} entries, {} bytes) to {}",
                    map.len(),
                    bytes.len(),
                    cache_path.display()
                );
            }
        }
        Err(e) => eprintln!("[cache] serialize tx_index: {e}"),
    }
    map
}

