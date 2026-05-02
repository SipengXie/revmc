//! Shared helpers for SkelOT paper binaries.
//!
//! Included via `#[path = "../skelot_common.rs"] mod skelot_common;` in each
//! binary under `examples/runner/src/bin/`, matching the `bin_common.rs`
//! pattern used elsewhere in this crate.

#![allow(dead_code)]

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use revm::bytecode::{Bytecode, JumpTable};
use revm::primitives::hardfork::SpecId;
use revm::primitives::{Address, Bytes, B256};
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
}

impl ChainId {
    pub fn from_chain_id(id: u64) -> Self {
        match id {
            8453 => ChainId::Base,
            1 => ChainId::Eth,
            56 => ChainId::Bsc,
            other => panic!("unsupported chain_id {other}; expected 1, 56, or 8453"),
        }
    }

    pub fn as_lowercase(&self) -> &'static str {
        match self {
            ChainId::Base => "base",
            ChainId::Eth => "eth",
            ChainId::Bsc => "bsc",
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
            _ => Err(format!("unknown chain '{s}', expected base|eth|bsc")),
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

/// Extract the opcode skeleton from EVM bytecode.
/// Keeps all opcodes but strips PUSH1-PUSH32 immediate bytes.
pub fn extract_opcode_skeleton(bytes: &[u8]) -> Vec<u8> {
    let mut skeleton = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let op = bytes[i];
        skeleton.push(op);
        i += 1;
        if (0x60..=0x7f).contains(&op) {
            i += (op - 0x5f) as usize;
        }
    }
    skeleton
}

/// Same as `extract_opcode_skeleton` but also returns `(opcode_count, imm_bytes)`
/// for Stage A's Table 1 bucket statistics.
pub fn extract_opcode_skeleton_with_counts(bytes: &[u8]) -> (Vec<u8>, usize, usize) {
    let mut skeleton = Vec::with_capacity(bytes.len());
    let mut opcode_count = 0usize;
    let mut imm_bytes = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        let op = bytes[i];
        skeleton.push(op);
        opcode_count += 1;
        i += 1;
        if (0x60..=0x7f).contains(&op) {
            let n = (op - 0x5f) as usize;
            imm_bytes += n;
            i += n;
        }
    }
    (skeleton, opcode_count, imm_bytes)
}

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

// ── Protocol identification (lifted from skeleton_identify.rs) ─────────────

pub struct KnownSelector {
    pub bytes: [u8; 4],
    pub name: &'static str,
}

pub const KNOWN_SELECTORS: &[KnownSelector] = &[
    // ERC-20
    KnownSelector { bytes: [0xa9, 0x05, 0x9c, 0xbb], name: "transfer(address,uint256)" },
    KnownSelector {
        bytes: [0x23, 0xb8, 0x72, 0xdd],
        name: "transferFrom(address,address,uint256)",
    },
    KnownSelector { bytes: [0x09, 0x5e, 0xa7, 0xb3], name: "approve(address,uint256)" },
    KnownSelector { bytes: [0x70, 0xa0, 0x82, 0x31], name: "balanceOf(address)" },
    KnownSelector { bytes: [0x18, 0x16, 0x0d, 0xdd], name: "totalSupply()" },
    KnownSelector { bytes: [0xdd, 0x62, 0xed, 0x3e], name: "allowance(address,address)" },
    // ERC-721
    KnownSelector {
        bytes: [0x42, 0x84, 0x2e, 0x0e],
        name: "safeTransferFrom(address,address,uint256)",
    },
    KnownSelector { bytes: [0x63, 0x52, 0x21, 0x1e], name: "ownerOf(uint256)" },
    // Uniswap V2
    KnownSelector {
        bytes: [0x02, 0x2c, 0x0d, 0x9f],
        name: "UniV2:swap(uint256,uint256,address,bytes)",
    },
    KnownSelector { bytes: [0x09, 0x02, 0xf1, 0xac], name: "UniV2:getReserves()" },
    KnownSelector { bytes: [0x6a, 0x62, 0x78, 0x42], name: "UniV2:mint(address)" },
    KnownSelector { bytes: [0xd2, 0x12, 0x20, 0xa7], name: "UniV2:factory()" },
    // Uniswap V3
    KnownSelector {
        bytes: [0x12, 0x8a, 0xcb, 0x08],
        name: "UniV3:swap(address,bool,int256,uint160,bytes)",
    },
    KnownSelector { bytes: [0x3c, 0x8a, 0x7d, 0x8d], name: "UniV3:collect(CollectParams)" },
    KnownSelector { bytes: [0x25, 0x14, 0x00, 0x00], name: "UniV3:positions(uint256)" },
    KnownSelector { bytes: [0xf3, 0x05, 0x83, 0x99], name: "UniV3:observe(uint32[])" },
    KnownSelector { bytes: [0x1a, 0x68, 0x65, 0x02], name: "UniV3:slot0()" },
    KnownSelector {
        bytes: [0x49, 0x04, 0x88, 0x76],
        name: "UniV3:flash(address,uint256,uint256,bytes)",
    },
    // Uniswap V3 Router
    KnownSelector {
        bytes: [0xac, 0x96, 0x50, 0xd8],
        name: "UniV3Router:multicall(uint256,bytes[])",
    },
    KnownSelector {
        bytes: [0x04, 0xe4, 0x5a, 0xaf],
        name: "UniV3Router:exactInputSingle(ExactInputSingleParams)",
    },
    // Aave V3
    KnownSelector {
        bytes: [0xe8, 0xed, 0xa9, 0xdf],
        name: "Aave:supply(address,uint256,address,uint16)",
    },
    KnownSelector {
        bytes: [0x69, 0x32, 0x8d, 0xec],
        name: "Aave:withdraw(address,uint256,address)",
    },
    KnownSelector {
        bytes: [0xa4, 0x15, 0xbc, 0xad],
        name: "Aave:borrow(address,uint256,uint256,uint16,address)",
    },
    KnownSelector {
        bytes: [0x57, 0x3e, 0xad, 0x4c],
        name: "Aave:repay(address,uint256,uint256,address)",
    },
    // Compound
    KnownSelector { bytes: [0xa0, 0x71, 0x2d, 0x68], name: "Compound:mint(uint256)" },
    KnownSelector { bytes: [0xdb, 0x00, 0x6a, 0x75], name: "Compound:redeem(uint256)" },
    // Proxy patterns
    KnownSelector { bytes: [0x5c, 0x60, 0xda, 0x1b], name: "Proxy:implementation()" },
    KnownSelector { bytes: [0xf8, 0x51, 0xa4, 0x40], name: "Proxy:admin()" },
    KnownSelector { bytes: [0x36, 0x59, 0xcf, 0xe6], name: "Proxy:upgradeTo(address)" },
    KnownSelector {
        bytes: [0x4f, 0x1e, 0xf2, 0x86],
        name: "Proxy:upgradeToAndCall(address,bytes)",
    },
    // OpenZeppelin AccessControl / Ownable
    KnownSelector { bytes: [0x8d, 0xa5, 0xcb, 0x5b], name: "OZ:renounceOwnership()" },
    KnownSelector { bytes: [0xf2, 0xfd, 0xe3, 0x8b], name: "OZ:transferOwnership(address)" },
    KnownSelector { bytes: [0x8d, 0xa5, 0xcb, 0x5b], name: "OZ:owner()" },
    // Safe (Gnosis Safe)
    KnownSelector { bytes: [0x6a, 0x76, 0x12, 0x02], name: "Safe:execTransaction(...)" },
    KnownSelector { bytes: [0xaf, 0xfe, 0xd0, 0xe0], name: "Safe:nonce()" },
    // Multicall
    KnownSelector { bytes: [0xac, 0x96, 0x50, 0xd8], name: "Multicall:multicall(uint256,bytes[])" },
    KnownSelector { bytes: [0x25, 0x2d, 0xba, 0x42], name: "Multicall:multicall(bytes[])" },
    // Curve
    KnownSelector {
        bytes: [0x3d, 0xf0, 0x21, 0x24],
        name: "Curve:exchange(int128,int128,uint256,uint256)",
    },
    // 1inch
    KnownSelector {
        bytes: [0x12, 0xaa, 0x3c, 0xaf],
        name: "1inch:swap(IAggregationExecutor,SwapDescription,bytes,bytes)",
    },
    // EIP-4337 Account Abstraction
    KnownSelector {
        bytes: [0x1f, 0xad, 0x94, 0x8c],
        name: "AA:validateUserOp(UserOperation,bytes32,uint256)",
    },
    KnownSelector { bytes: [0xb6, 0x1d, 0x27, 0xf6], name: "AA:execute(address,uint256,bytes)" },
    KnownSelector {
        bytes: [0x18, 0xdf, 0xb3, 0xc7],
        name: "AA:handleOps(UserOperation[],address)",
    },
    // Create2 / Deployer
    KnownSelector {
        bytes: [0xcd, 0xcb, 0x76, 0x0a],
        name: "Create2:deploy(uint256,bytes32,bytes)",
    },
];

/// Scan bytecode for known 4-byte selectors (checking PUSH4 opcodes).
pub fn find_selectors_in_bytecode(bytes: &[u8]) -> Vec<&'static str> {
    let mut found = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let op = bytes[i];
        i += 1;
        if (0x60..=0x7f).contains(&op) {
            let n = (op - 0x5f) as usize;
            if op == 0x63 && i + 4 <= bytes.len() {
                let sel: [u8; 4] = bytes[i..i + 4].try_into().unwrap();
                for ks in KNOWN_SELECTORS {
                    if ks.bytes == sel && !found.contains(&ks.name) {
                        found.push(ks.name);
                    }
                }
            }
            i += n;
        }
    }
    found
}

/// Scan bytecode for ANY 4-byte selectors via PUSH4, return unique hex strings.
pub fn find_all_push4_selectors(bytes: &[u8]) -> Vec<String> {
    let mut selectors = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut i = 0;
    while i < bytes.len() {
        let op = bytes[i];
        i += 1;
        if (0x60..=0x7f).contains(&op) {
            let n = (op - 0x5f) as usize;
            if op == 0x63 && i + 4 <= bytes.len() {
                let sel: [u8; 4] = bytes[i..i + 4].try_into().unwrap();
                if seen.insert(sel) {
                    selectors.push(hex::encode(sel));
                }
            }
            i += n;
        }
    }
    selectors
}

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

// ── Bench-data bytecode loader (shared by Stage B / Stage E) ──────────────

use revm::primitives::{HashMap as RevmHashMap, U256};
use revm::state::AccountInfo;

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

pub fn iter_legacy_bytecode<F: FnMut(&B256, Vec<u8>)>(snap: &CacheSnapshotV1, mut f: F) {
    for (hash, code) in &snap.codes {
        let raw = match code.clone() {
            BytecodeV86Mirror::LegacyAnalyzed { bytecode, .. } => bytecode.to_vec(),
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
    for bn in start_block..start_block + block_count {
        let path = bench_dir.join(format!("states/{bn}.bin"));
        let Ok(data) = std::fs::read(&path) else { continue };
        let Ok(mut snapshot): Result<CacheSnapshot, _> = bincode::deserialize(&data) else {
            continue;
        };
        let codes = std::mem::take(&mut snapshot.codes);
        drop(snapshot);
        for (hash, bytecode) in codes {
            let raw = bytecode.original_byte_slice().to_vec();
            if should_skip_bytes(&raw) {
                continue;
            }
            all.entry(hash).or_insert(raw);
        }
    }
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
    let cache_path = cache_dir.join(format!("bytecodes_{start_block}_{block_count}.bin"));
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

// ── Statistics (Welch's t) ─────────────────────────────────────────────────

/// Welch's two-sample t-test. Returns `(t, p)` with a two-sided p-value
/// computed from the normal approximation (adequate for rounds >= ~30).
pub fn welch_t(a: &[f64], b: &[f64]) -> (f64, f64) {
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    let var = |v: &[f64], m: f64| {
        v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (v.len() as f64 - 1.0).max(1.0)
    };
    let ma = mean(a);
    let mb = mean(b);
    let va = var(a, ma);
    let vb = var(b, mb);
    let se = ((va / a.len() as f64) + (vb / b.len() as f64)).sqrt();
    if se == 0.0 {
        return (0.0, 1.0);
    }
    let t = (ma - mb) / se;
    let z = t.abs();
    let p = 2.0 * (1.0 - 0.5 * (1.0 + libm::erf(z / std::f64::consts::SQRT_2)));
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
    pub skeleton_raw_len: usize,
    pub member_count: usize,
    pub member_hashes: Vec<String>,
}

/// Exact bucket boundaries used by Stage A.
/// Returns 0..=4 for tiny/small/medium/large/huge.
fn bucket_index_for_raw_len(raw_len: usize) -> usize {
    match raw_len {
        0..=99 => 0,
        100..=999 => 1,
        1_000..=4_999 => 2,
        5_000..=14_999 => 3,
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
        buckets[bucket_index_for_raw_len(fam.skeleton_raw_len)].push(fam);
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

    fn mk_family(skeleton_raw_len: usize, members: usize) -> FamilySummary {
        FamilySummary {
            skeleton_id: format!("fam-{}-{}", skeleton_raw_len, members),
            skeleton_raw_len,
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
            per_bucket[bucket_index_for_raw_len(fam.skeleton_raw_len)] += 1;
        }
        assert_eq!(per_bucket, [20, 20, 20, 20, 20]);
    }

    #[test]
    fn chain_id_from_chain_id_roundtrip() {
        use super::ChainId;
        assert_eq!(ChainId::from_chain_id(8453), ChainId::Base);
        assert_eq!(ChainId::from_chain_id(1), ChainId::Eth);
        assert_eq!(ChainId::from_chain_id(56), ChainId::Bsc);
    }

    #[test]
    fn chain_id_parses_lowercase_string() {
        use super::ChainId;
        use std::str::FromStr;
        assert_eq!(ChainId::from_str("base").unwrap(), ChainId::Base);
        assert_eq!(ChainId::from_str("eth").unwrap(), ChainId::Eth);
        assert_eq!(ChainId::from_str("bsc").unwrap(), ChainId::Bsc);
        assert!(ChainId::from_str("xyz").is_err());
    }

    #[test]
    fn spec_tag_for_chain_is_chain_only() {
        use super::{spec_tag_for_chain, ChainId};
        assert_eq!(spec_tag_for_chain(ChainId::Base), "base");
        assert_eq!(spec_tag_for_chain(ChainId::Eth), "eth");
        assert_eq!(spec_tag_for_chain(ChainId::Bsc), "bsc");
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
