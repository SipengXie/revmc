//! Bench-corpus decoding and loading shared by the SkelOT binaries:
//! v0/v1 state-snapshot schemas, manifest dispatch, and the cached
//! bytecode-corpus collector. Split out of `skelot_common.rs`, which
//! re-exports everything here.

use std::path::Path;

use revm::bytecode::{Bytecode, JumpTable};
use revm::primitives::{Address, Bytes, HashMap as RevmHashMap, B256, U256};
use revm::state::AccountInfo;
use serde::Deserialize;

use super::should_skip_bytes;

#[derive(Deserialize)]
pub struct AccountSnapshot {
    pub info: Option<AccountInfo>,
    pub storage: RevmHashMap<U256, U256>,
}

#[derive(Deserialize)]
pub struct CacheSnapshot {
    pub block_number: u64,
    pub has_state_clear: bool,
    pub accounts: RevmHashMap<Address, AccountSnapshot>,
    pub codes: RevmHashMap<B256, Bytecode>,
}

// ── v1 schema (BytecodeV86) explicit decode path ──────────────────────────

/// Mirror of `exporter::schema::BytecodeV86`. Kept local so we do not depend
/// on the exporter crate; converted to revm `Bytecode` via `From`.
#[derive(Clone, Debug, Deserialize)]
pub enum BytecodeV86Mirror {
    Eip7702 { delegated_address: Address, version: u8, raw: Bytes },
    LegacyAnalyzed { bytecode: Bytes, original_len: usize, jump_table: JumpTable },
}

impl From<BytecodeV86Mirror> for Bytecode {
    fn from(v: BytecodeV86Mirror) -> Self {
        match v {
            BytecodeV86Mirror::LegacyAnalyzed { bytecode, original_len, jump_table } => {
                Bytecode::new_analyzed(bytecode, original_len, jump_table)
            }
            // The exporter's `Bytecode::new_eip7702_raw` is not exposed by this
            // workspace's revm fork. `new_raw_checked` peeks the EIP-7702 magic
            // prefix and produces the same `Bytecode::Eip7702(...)` variant.
            BytecodeV86Mirror::Eip7702 { raw, .. } => {
                Bytecode::new_raw_checked(raw).expect("stored EIP-7702 bytecode must be valid")
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct V1AccountSnapshot {
    pub info: Option<AccountInfo>,
    pub storage: RevmHashMap<U256, U256>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CacheSnapshotV1 {
    pub block_number: u64,
    pub has_state_clear: bool,
    pub accounts: RevmHashMap<Address, V1AccountSnapshot>,
    pub codes: RevmHashMap<B256, BytecodeV86Mirror>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BenchManifest {
    pub schema_version: u32,
    pub chain_id: Option<u64>,
    pub start_block: Option<u64>,
    pub end_block: Option<u64>,
}

pub fn read_bench_manifest(bench_dir: &std::path::Path) -> Option<BenchManifest> {
    let p = bench_dir.join("manifest.json");
    let raw = std::fs::read_to_string(&p).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Decode a bench-data state file. Always goes through `BytecodeV86` then
/// converts to revm `Bytecode`, removing reliance on enum-tag coincidence.
pub fn load_state_v1(path: &std::path::Path) -> Result<CacheSnapshotV1, String> {
    let raw = std::fs::read(path).map_err(|e| format!("read {path:?}: {e}"))?;
    bincode::deserialize::<CacheSnapshotV1>(&raw).map_err(|e| format!("decode {path:?}: {e}"))
}

pub fn iter_bytecode_raw<F: FnMut(&B256, Vec<u8>)>(snap: &CacheSnapshotV1, mut f: F) {
    for (hash, code) in &snap.codes {
        let raw = match code {
            // LegacyAnalyzed.bytecode includes 33 trailing bytes of safety padding
            // appended during analysis (32 zero bytes + STOP). Slice to original_len so
            // callers see only on-chain raw bytes — otherwise raw_len inflates by 33 and
            // skeleton extraction at end-of-code mid-PUSH boundaries diverges from the
            // original-bytes semantics used by the canonical paper data.
            BytecodeV86Mirror::LegacyAnalyzed { bytecode, original_len, .. } => {
                bytecode[..(*original_len).min(bytecode.len())].to_vec()
            }
            BytecodeV86Mirror::Eip7702 { raw, .. } => raw.to_vec(),
        };
        f(hash, raw);
    }
}

/// Shared cache directory for derived indices over the bench corpus.
/// Falls back to `/tmp/skelot_index` when `$HOME` is unset.
pub fn index_cache_dir() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".cache").join("skelot_index"))
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp/skelot_index"))
}

/// Gather non-skippable bytecodes from `states/{bn}.bin` snapshots over a block range.
pub fn collect_bytecodes(
    bench_dir: &Path,
    start_block: u64,
    block_count: u64,
) -> std::collections::HashMap<B256, Vec<u8>> {
    let mut all: std::collections::HashMap<B256, Vec<u8>> = std::collections::HashMap::new();
    let (mut unreadable, mut undecodable, mut skipped_codes) = (0u64, 0u64, 0u64);
    for bn in start_block..start_block + block_count {
        let path = bench_dir.join(format!("states/{bn}.bin"));
        let Ok(data) = std::fs::read(&path) else {
            unreadable += 1;
            continue;
        };
        let Ok(mut snapshot): Result<CacheSnapshot, _> = bincode::deserialize(&data) else {
            undecodable += 1;
            continue;
        };
        let codes = std::mem::take(&mut snapshot.codes);
        drop(snapshot);
        for (hash, bytecode) in codes {
            // Clamp like iter_bytecode_raw: serde fills the analyzed fields
            // directly (no constructor assert), so a corrupt original_len
            // must not become an out-of-bounds slice under panic=abort.
            let raw: &[u8] = match &bytecode {
                Bytecode::LegacyAnalyzed(a) => {
                    let b = a.bytecode();
                    &b[..a.original_len().min(b.len())]
                }
                other => other.original_byte_slice(),
            };
            if should_skip_bytes(raw) {
                skipped_codes += 1;
                continue;
            }
            // Copy only on first sight: a contract loaded by N blocks would
            // otherwise be copied N times and dropped N-1 times.
            all.entry(hash).or_insert_with(|| raw.to_vec());
        }
    }
    if unreadable + undecodable > 0 {
        eprintln!(
            "  WARN: corpus scan skipped {unreadable} unreadable + {undecodable} undecodable state files (of {block_count})"
        );
    }
    eprintln!("  corpus filter: {skipped_codes} code entries excluded (0xEF prefix / empty)");
    all
}

/// Cached wrapper around `collect_bytecodes`. Reads from
/// `~/.cache/skelot_index/bytecodes_{start_block}_{block_count}.bin` when
/// present, otherwise rebuilds and atomically writes the cache. The bench
/// corpus is assumed static; remove the cache file if it changes.
pub fn collect_bytecodes_cached(
    bench_dir: &Path,
    start_block: u64,
    block_count: u64,
) -> std::collections::HashMap<B256, Vec<u8>> {
    let cache_dir = index_cache_dir();
    // Key includes the corpus directory name: two chains (or corpus
    // versions) over the same block window must never share a cache file.
    let corpus_tag = bench_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "corpus".into());
    let cache_path =
        cache_dir.join(format!("bytecodes_{corpus_tag}_{start_block}_{block_count}.bin"));
    if let Ok(data) = std::fs::read(&cache_path) {
        match bincode::deserialize::<std::collections::HashMap<B256, Vec<u8>>>(&data) {
            Ok(map) => {
                eprintln!(
                    "[cache] loaded bytecodes ({} entries) from {}",
                    map.len(),
                    cache_path.display()
                );
                return map;
            }
            Err(e) => eprintln!("[cache] {} unreadable ({e}); rebuilding", cache_path.display()),
        }
    }
    let map = collect_bytecodes(bench_dir, start_block, block_count);
    let _ = std::fs::create_dir_all(&cache_dir);
    match bincode::serialize(&map) {
        Ok(bytes) => {
            let tmp = cache_path.with_extension("bin.tmp");
            if std::fs::write(&tmp, &bytes).is_ok() && std::fs::rename(&tmp, &cache_path).is_ok() {
                eprintln!(
                    "[cache] wrote bytecodes ({} entries, {} bytes) to {}",
                    map.len(),
                    bytes.len(),
                    cache_path.display()
                );
            }
        }
        Err(e) => eprintln!("[cache] serialize bytecodes: {e}"),
    }
    map
}

