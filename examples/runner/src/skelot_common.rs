//! Shared helpers for SkelOT paper binaries.
//!
//! Included via `#[path = "../skelot_common.rs"] mod skelot_common;` in each
//! binary under `examples/runner/src/bin/`, matching the `bin_common.rs`
//! pattern used elsewhere in this crate.

#![allow(dead_code)]

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use revm::bytecode::Bytecode;
use revm::primitives::hardfork::SpecId;
use revm::primitives::{Address, B256};
use revmc::skeleton::SkeletonVariance;
use revmc::{EvmCompiler, EvmLlvmBackend, OptimizationLevel};
use serde::{Deserialize, Serialize};

use op_revm::OpSpecId;

pub const OP_SPEC: OpSpecId = OpSpecId::ISTHMUS;
pub const ETH_SPEC: SpecId = OP_SPEC.into_eth_spec();

// ── Chain identity ─────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainId {
    Base,
    Eth,
    Bsc,
    Arb,
}

impl ChainId {
    pub fn from_chain_id(id: u64) -> Self {
        match id {
            8453 => ChainId::Base,
            1 => ChainId::Eth,
            56 => ChainId::Bsc,
            42161 => ChainId::Arb,
            other => panic!("unsupported chain_id {other}; expected 1, 56, 8453, or 42161"),
        }
    }

    pub fn as_lowercase(&self) -> &'static str {
        match self {
            ChainId::Base => "base",
            ChainId::Eth => "eth",
            ChainId::Bsc => "bsc",
            ChainId::Arb => "arb",
        }
    }
}

impl std::str::FromStr for ChainId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "base" => Ok(ChainId::Base),
            "eth" | "ethereum" | "mainnet" => Ok(ChainId::Eth),
            "bsc" | "bnb" => Ok(ChainId::Bsc),
            "arb" | "arbitrum" => Ok(ChainId::Arb),
            _ => Err(format!("unknown chain '{s}', expected base|eth|bsc|arb")),
        }
    }
}

// ── Spec / opt tags ────────────────────────────────────────────────────────

pub fn spec_tag() -> String {
    spec_tag_for_chain(ChainId::Base)
}

/// Chain-only tag, used as a cache prefix for static analyses.
/// Static analyses do not execute opcodes, so deriving any fork name from
/// `OP_SPEC` would falsely imply fork-correct compilation cross-chain. Any
/// future dynamic measurement task should introduce a separate fork-aware
/// tag (e.g., `spec_tag_for_dynamic(chain, fork)`).
pub fn spec_tag_for_chain(chain: ChainId) -> String {
    chain.as_lowercase().to_string()
}

/// Default corpus location for the dynamic-analysis binaries
/// (`push_variance`, `skeleton_deployment_span`): `(bench_dir, start_block,
/// block_count)`. `skelot_dataset_scan` keeps its own table — its Eth/BSC
/// windows and Base block count differ on purpose.
pub fn chain_data_defaults(chain: ChainId) -> (&'static str, u64, u64) {
    match chain {
        ChainId::Base => ("/home/ubuntu/sipeng/bench_data_base", 38_004_930, 9_997),
        ChainId::Eth => ("/home/ubuntu/sipeng/bench_data_eth", 23_000_000, 10_000),
        ChainId::Bsc => ("/home/ubuntu/sipeng/bench_data_bsc", 95_897_884, 10_000),
        ChainId::Arb => ("/home/ubuntu/sipeng/bench_data_arb", 458_863_320, 10_000),
    }
}

// ── Size buckets (paper Table 1) ───────────────────────────────────────────

/// Provenance tag embedded in dataset_scan.json so any consumer can detect
/// which bucket boundaries produced a given file. Bumped only when boundaries
/// change.
pub const BUCKET_SCHEME_ID: &str = "paper_v1_small_100_999";

pub fn bucket_for_raw_len(len: usize) -> &'static str {
    match len {
        0..=99 => "tiny",
        100..=999 => "small",
        1_000..=4_999 => "medium",
        5_000..=14_999 => "large",
        _ => "huge",
    }
}

pub fn opt_tag(opt: OptimizationLevel) -> &'static str {
    match opt {
        OptimizationLevel::None => "o0",
        OptimizationLevel::Less => "o1",
        OptimizationLevel::Default => "o2",
        OptimizationLevel::Aggressive => "o3",
    }
}

// ── Skeleton extraction ────────────────────────────────────────────────────

#[allow(unused_imports)]
pub use revmc::skeleton::extract_opcode_skeleton;

/// Same as `extract_opcode_skeleton` but also returns `(opcode_count, imm_bytes)`
/// for Stage A's Table 1 bucket statistics.
pub fn extract_opcode_skeleton_with_counts(bytes: &[u8]) -> (Vec<u8>, usize, usize) {
    let skeleton = extract_opcode_skeleton(bytes);
    // Every byte is either an opcode or an immediate, so the counts are
    // derivable. Counting the DECLARED push width instead would overshoot on a
    // bytecode whose final PUSH is truncated, making opcodes+immediates exceed
    // the raw length.
    let opcode_count = skeleton.len();
    let imm_bytes = bytes.len() - opcode_count;
    (skeleton, opcode_count, imm_bytes)
}

/// EMSE-boundary helpers live in the core crate so skeleton compilation and
/// analysis binaries share one implementation (see `revmc::emse`).
#[allow(unused_imports)]
pub use revmc::emse::{extract_emse_skeleton, strip_emse_suffixes};

/// FNV-style hash using `DefaultHasher`. Matches existing
/// `compile_hex.rs`/`opcode_dedup.rs` hashing semantics.
pub fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

// ── Skip filters ───────────────────────────────────────────────────────────

/// Filter for bytecodes that `revmc` must not compile.
/// Bytecodes starting with `0xEF` are EIP-7702 delegated or legacy INVALID;
/// compiling them breaks gas semantics (see memory `eip7702-gas-bug`).
pub fn should_skip_bytes(bytes: &[u8]) -> bool {
    bytes.first() == Some(&0xEF) || bytes.is_empty()
}

pub fn should_skip_compilation(bc: &Bytecode) -> bool {
    let bytes = bc.original_byte_slice();
    should_skip_bytes(bytes)
}

// ── Per-hash compile ───────────────────────────────────────────────────────

pub struct PerHashArtifacts {
    pub symbol: String,
    pub object: PathBuf,
    pub library: PathBuf,
}

pub fn per_hash_paths(cache_dir: &Path, hash: &B256, opt: OptimizationLevel) -> PerHashArtifacts {
    let hash_hex = hex::encode(hash);
    let key = format!("{hash_hex}__{}__{}", spec_tag(), opt_tag(opt));
    let stem = cache_dir.join(&key);
    PerHashArtifacts {
        symbol: format!("c_{hash_hex}"),
        object: stem.with_extension("o"),
        library: stem.with_extension("so"),
    }
}

pub fn compile_per_hash(
    bytecode: &[u8],
    symbol: &str,
    obj: &Path,
    lib: &Path,
    opt: OptimizationLevel,
) -> Result<(), String> {
    if let Some(parent) = obj.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let ctx_ptr = Box::into_raw(Box::new(revmc::llvm::inkwell::context::Context::create()));
    let result = (|| {
        let context = unsafe { &*ctx_ptr };
        let backend =
            EvmLlvmBackend::new(context, true, opt).map_err(|e| format!("backend: {e}"))?;
        let mut compiler = EvmCompiler::new(backend);
        compiler.translate(symbol, bytecode, ETH_SPEC).map_err(|e| format!("translate: {e}"))?;
        compiler.write_object_to_file(obj).map_err(|e| format!("write: {e}"))?;
        revmc::Linker::new().link(lib, [obj]).map_err(|e| format!("link: {e}"))?;
        Ok::<(), String>(())
    })();
    unsafe {
        drop(Box::from_raw(ctx_ptr));
    }
    result
}

// ── Skeleton compile ───────────────────────────────────────────────────────

pub struct SkeletonArtifacts {
    pub symbol: String,
    pub object: PathBuf,
    pub library: PathBuf,
}

pub fn skeleton_paths(
    cache_dir: &Path,
    skel_hash: u64,
    opt: OptimizationLevel,
) -> SkeletonArtifacts {
    let key = format!("skel_{skel_hash:016x}__{}__{}", spec_tag(), opt_tag(opt));
    let stem = cache_dir.join(&key);
    SkeletonArtifacts {
        symbol: format!("skel_{skel_hash:016x}"),
        object: stem.with_extension("o"),
        library: stem.with_extension("so"),
    }
}

pub fn compile_skeleton_group(
    bytecode: &[u8],
    variance: &SkeletonVariance,
    symbol: &str,
    obj: &Path,
    lib: &Path,
    opt: OptimizationLevel,
) -> Result<(), String> {
    if let Some(parent) = obj.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let ctx_ptr = Box::into_raw(Box::new(revmc::llvm::inkwell::context::Context::create()));
    let result = (|| {
        let context = unsafe { &*ctx_ptr };
        let backend =
            EvmLlvmBackend::new(context, true, opt).map_err(|e| format!("backend: {e}"))?;
        let mut compiler = EvmCompiler::new(backend);
        compiler
            .translate_skeleton(symbol, bytecode, ETH_SPEC, variance)
            .map_err(|e| format!("translate_skeleton: {e}"))?;
        compiler.write_object_to_file(obj).map_err(|e| format!("write: {e}"))?;
        revmc::Linker::new().link(lib, [obj]).map_err(|e| format!("link: {e}"))?;
        Ok::<(), String>(())
    })();
    unsafe {
        drop(Box::from_raw(ctx_ptr));
    }
    result
}

// ── Protocol identification (identify_pattern; used by dataset scan) ──────

/// Identify a contract pattern (EIP-1167, Uniswap, ERC-20, etc.) from raw bytecode.
pub fn identify_pattern(bytes: &[u8]) -> &'static str {
    if bytes.is_empty() {
        return "empty";
    }

    // EIP-1167 minimal proxy: 363d3d373d3d3d363d73...5af43d82803e903d91602b57fd5bf3
    if bytes.len() == 45 && bytes.starts_with(&[0x36, 0x3d, 0x3d, 0x37]) {
        return "EIP-1167 minimal proxy (45B)";
    }

    // EIP-1167 variant with 3d prefix
    if bytes.len() <= 50
        && (bytes.starts_with(&[0x36, 0x3d]) || bytes.starts_with(&[0x3d, 0x3d]))
        && bytes.contains(&0xf4)
    {
        return "EIP-1167 minimal proxy variant";
    }

    // EIP-2535 Diamond proxy pattern: usually has diamondCut, facets, facetAddress selectors
    // facetAddress: 0xcdffacc6, diamondCut: 0x1f931c1c
    if bytes.len() > 1000 && contains_bytes(bytes, &[0xcd, 0xff, 0xac, 0xc6]) {
        return "EIP-2535 Diamond proxy";
    }

    // Transparent / UUPS upgradeable proxy: has implementation(), admin(), upgradeTo()
    let has_implementation = contains_bytes(bytes, &[0x5c, 0x60, 0xda, 0x1b]);
    let has_upgrade_to = contains_bytes(bytes, &[0x36, 0x59, 0xcf, 0xe6]);
    if has_implementation && has_upgrade_to {
        return "Transparent/UUPS upgradeable proxy";
    }

    // UUPS / Transparent proxy: short, DELEGATECALL-based
    if bytes.len() < 200 && bytes.contains(&0xf4) {
        return "small DELEGATECALL proxy";
    }

    // Check for Uniswap V3 Pool: swap(0x128acb08), slot0(0x1a686502), observe(0xf3058399)
    let has_uni_v3_swap = contains_bytes(bytes, &[0x12, 0x8a, 0xcb, 0x08]);
    let has_uni_v3_slot0 = contains_bytes(bytes, &[0x1a, 0x68, 0x65, 0x02]);
    if has_uni_v3_swap && has_uni_v3_slot0 {
        return "Uniswap V3 Pool";
    }

    // Uniswap V2 Pair: swap(0x022c0d9f), getReserves(0x0902f1ac)
    let has_uni_v2_swap = contains_bytes(bytes, &[0x02, 0x2c, 0x0d, 0x9f]);
    let has_uni_v2_reserves = contains_bytes(bytes, &[0x09, 0x02, 0xf1, 0xac]);
    if has_uni_v2_swap && has_uni_v2_reserves {
        return "Uniswap V2 Pair";
    }

    // ERC-20 token: transfer, balanceOf, approve
    let has_transfer = contains_bytes(bytes, &[0xa9, 0x05, 0x9c, 0xbb]);
    let has_balance = contains_bytes(bytes, &[0x70, 0xa0, 0x82, 0x31]);
    let has_approve = contains_bytes(bytes, &[0x09, 0x5e, 0xa7, 0xb3]);
    if has_transfer && has_balance && has_approve {
        // Might also be ERC-721 if it has ownerOf
        let has_owner_of = contains_bytes(bytes, &[0x63, 0x52, 0x21, 0x1e]);
        if has_owner_of {
            return "ERC-721 token (with ERC-20 interface)";
        }
        return "ERC-20 token";
    }

    // Aave aToken / lending pool
    let has_supply = contains_bytes(bytes, &[0xe8, 0xed, 0xa9, 0xdf]);
    let has_borrow = contains_bytes(bytes, &[0xa4, 0x15, 0xbc, 0xad]);
    if has_supply && has_borrow {
        return "Aave V3 Lending Pool";
    }

    // EIP-4337 Account (EntryPoint or SmartAccount)
    let has_validate_userop = contains_bytes(bytes, &[0x1f, 0xad, 0x94, 0x8c]);
    let has_handle_ops = contains_bytes(bytes, &[0x18, 0xdf, 0xb3, 0xc7]);
    let has_aa_execute = contains_bytes(bytes, &[0xb6, 0x1d, 0x27, 0xf6]);
    if has_validate_userop {
        return "EIP-4337 Smart Account";
    }
    if has_handle_ops {
        return "EIP-4337 EntryPoint";
    }
    if has_aa_execute {
        return "EIP-4337 Smart Account (execute)";
    }

    // Gnosis Safe / Safe
    let has_exec_tx = contains_bytes(bytes, &[0x6a, 0x76, 0x12, 0x02]);
    if has_exec_tx {
        return "Gnosis Safe";
    }

    // GnosisSafeProxy (very short, just fallback with DELEGATECALL)
    if bytes.len() < 100 && bytes.contains(&0xf4) && bytes.contains(&0x54) {
        return "Safe proxy (short DELEGATECALL+SLOAD)";
    }

    // Simple STOP / INVALID / REVERT only contracts
    if bytes.len() <= 2 {
        return "trivial (<=2 bytes)";
    }
    if bytes == [0x00] {
        return "STOP-only";
    }
    if bytes == [0xfe] {
        return "INVALID-only";
    }

    // Analyze opcode composition for hints
    let mut has_delegatecall = false;
    let mut has_create2 = false;
    let mut sload_count = 0u32;
    let mut _call_count = 0u32;
    let mut _log_count = 0u32;
    let mut opcode_count = 0u32;
    let mut i = 0;
    while i < bytes.len() {
        let op = bytes[i];
        opcode_count += 1;
        match op {
            0x54 => sload_count += 1,
            0xf1 | 0xf2 | 0xfa => _call_count += 1,
            0xf4 => {
                _call_count += 1;
                has_delegatecall = true;
            }
            0xf5 => has_create2 = true,
            0xa0..=0xa4 => _log_count += 1,
            _ => {}
        }
        i += 1;
        if (0x60..=0x7f).contains(&op) {
            i += (op - 0x5f) as usize;
        }
    }

    if has_create2 && bytes.len() > 5000 {
        return "factory (contains CREATE2)";
    }
    if has_delegatecall && bytes.len() < 500 {
        return "proxy (DELEGATECALL)";
    }
    if opcode_count > 0 {
        let storage_pct = sload_count as f64 / opcode_count as f64;
        if storage_pct > 0.05 {
            return "storage-heavy contract";
        }
    }

    if bytes.len() > 20000 {
        return "large contract (>20KB)";
    }
    if bytes.len() > 5000 {
        return "medium contract (5-20KB)";
    }
    if bytes.len() > 500 {
        return "small contract (500B-5KB)";
    }

    "micro contract (<500B)"
}

/// Check if a 4-byte sequence appears as PUSH4 immediate data in the bytecode.
pub fn contains_bytes(bytecode: &[u8], needle: &[u8; 4]) -> bool {
    let mut i = 0;
    while i < bytecode.len() {
        let op = bytecode[i];
        i += 1;
        if (0x60..=0x7f).contains(&op) {
            let n = (op - 0x5f) as usize;
            if op == 0x63 && i + 4 <= bytecode.len() && &bytecode[i..i + 4] == needle {
                return true;
            }
            i += n;
        }
    }
    false
}

// ── Bench-data loading & decoding (see skelot_data.rs) ────────────────────

#[path = "skelot_data.rs"]
pub mod skelot_data;
#[allow(unused_imports)]
pub use skelot_data::*;

// ── Statistics (Welch's t) ─────────────────────────────────────────────────

#[path = "stats.rs"]
pub mod stats;

/// Welch's two-sample t-test. Returns `(t, p)` with a two-sided p-value from
/// the t distribution at the Welch-Satterthwaite degrees of freedom.
///
/// The normal approximation this replaced is only adequate from ~30 samples per
/// arm; the replay driver defaults to 5 rounds, where it understates p enough
/// to flip a significance verdict.
pub fn welch_t(a: &[f64], b: &[f64]) -> (f64, f64) {
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    let var = |v: &[f64], m: f64| {
        v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (v.len() as f64 - 1.0).max(1.0)
    };
    let (na, nb) = (a.len() as f64, b.len() as f64);
    let ma = mean(a);
    let mb = mean(b);
    let va = var(a, ma);
    let vb = var(b, mb);
    let se_sq = va / na + vb / nb;
    if se_sq == 0.0 {
        return (0.0, 1.0);
    }
    let t = (ma - mb) / se_sq.sqrt();
    let df_den =
        (va / na).powi(2) / (na - 1.0).max(1.0) + (vb / nb).powi(2) / (nb - 1.0).max(1.0);
    let df = if df_den > 0.0 { se_sq.powi(2) / df_den } else { (na + nb - 2.0).max(1.0) };
    let p = 2.0 * (1.0 - self::stats::t_cdf(t.abs(), df));
    (t, p)
}

/// Thin wrapper returning only the two-sided p-value from [`welch_t`].
pub fn welch_p(a: &[f64], b: &[f64]) -> f64 {
    welch_t(a, b).1
}

// ── Family summary + stratified sampling (Stage E) ────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FamilySummary {
    pub skeleton_id: String,
    /// Opcode-skeleton length (PUSH immediates stripped), NOT the raw
    /// bytecode length — the two differ by ~15-20% on Solidity output, so
    /// this must never be fed to `bucket_for_raw_len`.
    pub skeleton_len: usize,
    /// Size bucket as computed by Stage A from the RAW bytecode length.
    pub bucket: String,
    pub member_count: usize,
    pub member_hashes: Vec<String>,
}

/// Index of a bucket name produced by [`bucket_for_raw_len`].
/// Returns 0..=4 for tiny/small/medium/large/huge.
pub fn bucket_index(name: &str) -> usize {
    match name {
        "tiny" => 0,
        "small" => 1,
        "medium" => 2,
        "large" => 3,
        _ => 4,
    }
}

/// Return a stratified sample of `families`: from each of the 5 buckets, take
/// the top `per_bucket` families by `member_count` (ties broken by
/// lexicographic `skeleton_id`). Singleton families (`member_count < 2`) are
/// excluded — they collapse per_hash and SkelOT to identical points and
/// contribute no ablation signal. `seed` is reserved for future randomized
/// variants.
pub fn stratified_family_sample(
    families: &[FamilySummary],
    per_bucket: usize,
    _seed: u64,
) -> Vec<FamilySummary> {
    let mut buckets: [Vec<&FamilySummary>; 5] = Default::default();
    for fam in families {
        if fam.member_count < 2 {
            continue;
        }
        buckets[bucket_index(&fam.bucket)].push(fam);
    }
    let mut out = Vec::with_capacity(per_bucket * 5);
    for mut bucket in buckets {
        bucket.sort_by(|a, b| {
            b.member_count.cmp(&a.member_count).then_with(|| a.skeleton_id.cmp(&b.skeleton_id))
        });
        for fam in bucket.into_iter().take(per_bucket) {
            out.push(fam.clone());
        }
    }
    out
}

/// For EIP-1167: extract embedded implementation address (bytes 10..30).
pub fn eip1167_impl_addr(bytes: &[u8]) -> Option<Address> {
    if bytes.len() == 45 && bytes.starts_with(&[0x36, 0x3d, 0x3d, 0x37]) && bytes[9] == 0x73 {
        Some(Address::from_slice(&bytes[10..30]))
    } else {
        None
    }
}

#[cfg(test)]
mod stratified_tests {
    use super::*;

    fn mk_family(raw_len: usize, members: usize) -> FamilySummary {
        FamilySummary {
            skeleton_id: format!("fam-{}-{}", raw_len, members),
            skeleton_len: raw_len,
            bucket: bucket_for_raw_len(raw_len).to_string(),
            member_count: members,
            member_hashes: (0..members).map(|i| format!("{:064x}", i)).collect(),
        }
    }

    #[test]
    fn stratified_sample_is_deterministic_and_stratified() {
        let mut families = Vec::new();
        for (bucket_idx, &raw_len) in [20usize, 200, 1_000, 5_000, 20_000].iter().enumerate() {
            for i in 0..40 {
                families.push(mk_family(raw_len, 100 - i + bucket_idx));
            }
        }
        let s1 = stratified_family_sample(&families, 20, 42);
        let s2 = stratified_family_sample(&families, 20, 42);
        assert_eq!(s1, s2, "same seed must produce identical selection");
        assert_eq!(s1.len(), 100);

        let mut per_bucket = [0usize; 5];
        for fam in &s1 {
            per_bucket[bucket_index(&fam.bucket)] += 1;
        }
        assert_eq!(per_bucket, [20, 20, 20, 20, 20]);
    }

    #[test]
    fn chain_id_from_chain_id_roundtrip() {
        use super::ChainId;
        assert_eq!(ChainId::from_chain_id(8453), ChainId::Base);
        assert_eq!(ChainId::from_chain_id(1), ChainId::Eth);
        assert_eq!(ChainId::from_chain_id(56), ChainId::Bsc);
        assert_eq!(ChainId::from_chain_id(42161), ChainId::Arb);
    }

    #[test]
    fn chain_id_parses_lowercase_string() {
        use super::ChainId;
        use std::str::FromStr;
        assert_eq!(ChainId::from_str("base").unwrap(), ChainId::Base);
        assert_eq!(ChainId::from_str("eth").unwrap(), ChainId::Eth);
        assert_eq!(ChainId::from_str("bsc").unwrap(), ChainId::Bsc);
        assert_eq!(ChainId::from_str("arb").unwrap(), ChainId::Arb);
        assert_eq!(ChainId::from_str("arbitrum").unwrap(), ChainId::Arb);
        assert!(ChainId::from_str("xyz").is_err());
    }

    #[test]
    fn spec_tag_for_chain_is_chain_only() {
        use super::{spec_tag_for_chain, ChainId};
        assert_eq!(spec_tag_for_chain(ChainId::Base), "base");
        assert_eq!(spec_tag_for_chain(ChainId::Eth), "eth");
        assert_eq!(spec_tag_for_chain(ChainId::Bsc), "bsc");
        assert_eq!(spec_tag_for_chain(ChainId::Arb), "arb");
        let base = spec_tag_for_chain(ChainId::Base);
        assert!(
            !base.contains("isthmus") && !base.contains("prague"),
            "static tag must not encode any fork name: {base}"
        );
    }

    #[test]
    fn spec_tag_default_routes_to_base() {
        use super::{spec_tag, spec_tag_for_chain, ChainId};
        assert_eq!(spec_tag(), spec_tag_for_chain(ChainId::Base));
    }

    #[test]
    fn bucket_for_raw_len_paper_boundaries() {
        use super::bucket_for_raw_len;
        assert_eq!(bucket_for_raw_len(0), "tiny");
        assert_eq!(bucket_for_raw_len(99), "tiny");
        assert_eq!(bucket_for_raw_len(100), "small");
        assert_eq!(bucket_for_raw_len(999), "small");
        assert_eq!(bucket_for_raw_len(1000), "medium");
        assert_eq!(bucket_for_raw_len(4999), "medium");
        assert_eq!(bucket_for_raw_len(5000), "large");
        assert_eq!(bucket_for_raw_len(14999), "large");
        assert_eq!(bucket_for_raw_len(15000), "huge");
        assert_eq!(bucket_for_raw_len(50000), "huge");
    }

    #[test]
    fn bucket_scheme_id_is_stable() {
        use super::BUCKET_SCHEME_ID;
        assert_eq!(BUCKET_SCHEME_ID, "paper_v1_small_100_999");
    }
}
