//! Identify protocols/contracts behind top-duplicated opcode skeletons.
//!
//! Loads a single block's state snapshot, computes opcode skeletons (same
//! algorithm as opcode_dedup.rs), groups by skeleton hash, and for groups
//! with 2+ members prints:
//!   - skeleton hash, count, bytecode size
//!   - first 64 bytes of raw bytecode (hex)
//!   - known function selectors found in the bytecode
//!   - pattern identification (EIP-1167 proxy, Uniswap V3, ERC-20, etc.)
//!
//! Usage:
//!   cargo run -p revmc-examples-runner --bin skeleton_identify --release [-- <block>]

use std::collections::HashMap;
use std::path::Path;

use revm::bytecode::Bytecode;
use revm::primitives::{Address, B256, HashMap as RevmHashMap, U256};
use revm::state::AccountInfo;
use serde::Deserialize;

// ── State deserialization (same as opcode_dedup.rs / scan_codes.rs) ─────────

#[derive(Deserialize)]
struct AccountSnapshot {
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
    accounts: RevmHashMap<Address, AccountSnapshot>,
    codes: RevmHashMap<B256, Bytecode>,
}

// ── Opcode skeleton (identical to opcode_dedup.rs) ──────────────────────────

/// Strip PUSH1-PUSH32 immediate bytes, keeping only opcodes.
fn extract_opcode_skeleton(bytes: &[u8]) -> Vec<u8> {
    let mut skeleton = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let op = bytes[i];
        skeleton.push(op);
        i += 1;
        if op >= 0x60 && op <= 0x7f {
            // PUSH1..PUSH32: skip N immediate bytes
            let n = (op - 0x5f) as usize;
            i += n;
        }
    }
    skeleton
}

/// Hash a byte slice with DefaultHasher (same as opcode_dedup.rs).
fn hash_bytes(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

// ── Known 4-byte function selectors ─────────────────────────────────────────

struct KnownSelector {
    bytes: [u8; 4],
    name: &'static str,
}

const KNOWN_SELECTORS: &[KnownSelector] = &[
    // ERC-20
    KnownSelector { bytes: [0xa9, 0x05, 0x9c, 0xbb], name: "transfer(address,uint256)" },
    KnownSelector { bytes: [0x23, 0xb8, 0x72, 0xdd], name: "transferFrom(address,address,uint256)" },
    KnownSelector { bytes: [0x09, 0x5e, 0xa7, 0xb3], name: "approve(address,uint256)" },
    KnownSelector { bytes: [0x70, 0xa0, 0x82, 0x31], name: "balanceOf(address)" },
    KnownSelector { bytes: [0x18, 0x16, 0x0d, 0xdd], name: "totalSupply()" },
    KnownSelector { bytes: [0xdd, 0x62, 0xed, 0x3e], name: "allowance(address,address)" },
    // ERC-721
    KnownSelector { bytes: [0x42, 0x84, 0x2e, 0x0e], name: "safeTransferFrom(address,address,uint256)" },
    KnownSelector { bytes: [0x63, 0x52, 0x21, 0x1e], name: "ownerOf(uint256)" },
    // Uniswap V2
    KnownSelector { bytes: [0x02, 0x2c, 0x0d, 0x9f], name: "UniV2:swap(uint256,uint256,address,bytes)" },
    KnownSelector { bytes: [0x09, 0x02, 0xf1, 0xac], name: "UniV2:getReserves()" },
    KnownSelector { bytes: [0x6a, 0x62, 0x78, 0x42], name: "UniV2:mint(address)" },
    KnownSelector { bytes: [0xd2, 0x12, 0x20, 0xa7], name: "UniV2:factory()" },
    // Uniswap V3
    KnownSelector { bytes: [0x12, 0x8a, 0xcb, 0x08], name: "UniV3:swap(address,bool,int256,uint160,bytes)" },
    KnownSelector { bytes: [0x3c, 0x8a, 0x7d, 0x8d], name: "UniV3:collect(CollectParams)" },
    KnownSelector { bytes: [0x25, 0x14, 0x00, 0x00], name: "UniV3:positions(uint256)" },
    KnownSelector { bytes: [0xf3, 0x05, 0x83, 0x99], name: "UniV3:observe(uint32[])" },
    KnownSelector { bytes: [0x1a, 0x68, 0x65, 0x02], name: "UniV3:slot0()" },
    KnownSelector { bytes: [0x49, 0x04, 0x88, 0x76], name: "UniV3:flash(address,uint256,uint256,bytes)" },
    // Uniswap V3 Router
    KnownSelector { bytes: [0xac, 0x96, 0x50, 0xd8], name: "UniV3Router:multicall(uint256,bytes[])" },
    KnownSelector { bytes: [0x04, 0xe4, 0x5a, 0xaf], name: "UniV3Router:exactInputSingle(ExactInputSingleParams)" },
    // Aave V3
    KnownSelector { bytes: [0xe8, 0xed, 0xa9, 0xdf], name: "Aave:supply(address,uint256,address,uint16)" },
    KnownSelector { bytes: [0x69, 0x32, 0x8d, 0xec], name: "Aave:withdraw(address,uint256,address)" },
    KnownSelector { bytes: [0xa4, 0x15, 0xbc, 0xad], name: "Aave:borrow(address,uint256,uint256,uint16,address)" },
    KnownSelector { bytes: [0x57, 0x3e, 0xad, 0x4c], name: "Aave:repay(address,uint256,uint256,address)" },
    // Compound
    KnownSelector { bytes: [0xa0, 0x71, 0x2d, 0x68], name: "Compound:mint(uint256)" },
    KnownSelector { bytes: [0xdb, 0x00, 0x6a, 0x75], name: "Compound:redeem(uint256)" },
    // Proxy patterns
    KnownSelector { bytes: [0x5c, 0x60, 0xda, 0x1b], name: "Proxy:implementation()" },
    KnownSelector { bytes: [0xf8, 0x51, 0xa4, 0x40], name: "Proxy:admin()" },
    KnownSelector { bytes: [0x36, 0x59, 0xcf, 0xe6], name: "Proxy:upgradeTo(address)" },
    KnownSelector { bytes: [0x4f, 0x1e, 0xf2, 0x86], name: "Proxy:upgradeToAndCall(address,bytes)" },
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
    KnownSelector { bytes: [0x3d, 0xf0, 0x21, 0x24], name: "Curve:exchange(int128,int128,uint256,uint256)" },
    // 1inch
    KnownSelector { bytes: [0x12, 0xaa, 0x3c, 0xaf], name: "1inch:swap(IAggregationExecutor,SwapDescription,bytes,bytes)" },
    // EIP-4337 Account Abstraction
    KnownSelector { bytes: [0x1f, 0xad, 0x94, 0x8c], name: "AA:validateUserOp(UserOperation,bytes32,uint256)" },
    KnownSelector { bytes: [0xb6, 0x1d, 0x27, 0xf6], name: "AA:execute(address,uint256,bytes)" },
    KnownSelector { bytes: [0x18, 0xdf, 0xb3, 0xc7], name: "AA:handleOps(UserOperation[],address)" },
    // Create2 / Deployer
    KnownSelector { bytes: [0xcd, 0xcb, 0x76, 0x0a], name: "Create2:deploy(uint256,bytes32,bytes)" },
];

/// Scan bytecode for known 4-byte selectors (checking PUSH4 opcodes).
fn find_selectors_in_bytecode(bytes: &[u8]) -> Vec<&'static str> {
    let mut found = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let op = bytes[i];
        i += 1;
        if op >= 0x60 && op <= 0x7f {
            let n = (op - 0x5f) as usize;
            // Check PUSH4 immediate data for known selectors
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
fn find_all_push4_selectors(bytes: &[u8]) -> Vec<String> {
    let mut selectors = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut i = 0;
    while i < bytes.len() {
        let op = bytes[i];
        i += 1;
        if op >= 0x60 && op <= 0x7f {
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

// ── Pattern identification ──────────────────────────────────────────────────

fn identify_pattern(bytes: &[u8]) -> &'static str {
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
            0xf4 => { _call_count += 1; has_delegatecall = true; }
            0xf5 => has_create2 = true,
            0xa0..=0xa4 => _log_count += 1,
            _ => {}
        }
        i += 1;
        if op >= 0x60 && op <= 0x7f {
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
fn contains_bytes(bytecode: &[u8], needle: &[u8; 4]) -> bool {
    let mut i = 0;
    while i < bytecode.len() {
        let op = bytecode[i];
        i += 1;
        if op >= 0x60 && op <= 0x7f {
            let n = (op - 0x5f) as usize;
            // Only check PUSH4 (0x63) immediates for selectors
            if op == 0x63 && i + 4 <= bytecode.len() {
                if &bytecode[i..i + 4] == needle {
                    return true;
                }
            }
            i += n;
        }
    }
    false
}

/// For EIP-1167: extract embedded implementation address (bytes 10..30).
fn eip1167_impl_addr(bytes: &[u8]) -> Option<Address> {
    if bytes.len() == 45 && bytes.starts_with(&[0x36, 0x3d, 0x3d, 0x37]) && bytes[9] == 0x73 {
        Some(Address::from_slice(&bytes[10..30]))
    } else {
        None
    }
}

// ── Skeleton group info ─────────────────────────────────────────────────────

struct SkeletonGroup {
    skeleton_hash: u64,
    skeleton_len: usize,
    /// (code_hash, raw bytecode bytes)
    members: Vec<(B256, Vec<u8>)>,
}

fn main() {
    let block: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(38004930);
    let bench_dir = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "/home/ubuntu/sipeng/bench_data".into());
    let bench_path = Path::new(&bench_dir);

    let states_path = bench_path.join(format!("states/{block}.bin"));
    eprintln!("Loading {states_path:?} ...");

    let data = std::fs::read(&states_path).expect("read state file");
    let snapshot: CacheSnapshot = bincode::deserialize(&data).expect("deserialize state");

    let n_accounts = snapshot.accounts.len();
    let n_codes = snapshot.codes.len();
    eprintln!(
        "Block {block}: {n_accounts} accounts, {n_codes} codes in snapshot"
    );

    // Build code_hash -> list of addresses
    let mut code_hash_to_addrs: HashMap<B256, Vec<Address>> = HashMap::new();
    for (addr, acc) in &snapshot.accounts {
        if let Some(ref info) = acc.info {
            if info.code_hash != revm::primitives::KECCAK_EMPTY {
                code_hash_to_addrs
                    .entry(info.code_hash)
                    .or_default()
                    .push(*addr);
            }
        }
    }

    // Phase 1: compute skeletons and group
    let mut groups: HashMap<u64, SkeletonGroup> = HashMap::new();

    for (code_hash, bytecode) in &snapshot.codes {
        let bytes = bytecode.original_byte_slice();
        if bytes.is_empty() {
            continue;
        }
        let skeleton = extract_opcode_skeleton(bytes);
        let skel_hash = hash_bytes(&skeleton);

        let group = groups.entry(skel_hash).or_insert_with(|| SkeletonGroup {
            skeleton_hash: skel_hash,
            skeleton_len: skeleton.len(),
            members: Vec::new(),
        });
        group.members.push((*code_hash, bytes.to_vec()));
    }

    // Sort by member count descending
    let mut sorted_groups: Vec<SkeletonGroup> = groups.into_values().collect();
    sorted_groups.sort_by_key(|g| std::cmp::Reverse(g.members.len()));

    // Print summary
    let total_non_empty = sorted_groups.iter().map(|g| g.members.len()).sum::<usize>();
    let dup_groups = sorted_groups.iter().filter(|g| g.members.len() >= 2).count();
    println!("=== Skeleton Identification Report (block {block}) ===");
    println!(
        "Non-empty bytecodes: {total_non_empty} | Unique skeletons: {} | Groups with 2+ members: {dup_groups}\n",
        sorted_groups.len()
    );

    // Show all groups with 2+ members, detailed for top 10
    let top_n = 10;
    for (rank, group) in sorted_groups.iter().enumerate() {
        if group.members.len() < 2 {
            break;
        }

        let count = group.members.len();
        let skel_hash_hex = format!("{:016x}", group.skeleton_hash);
        let representative = &group.members[0];
        let raw_bytes = &representative.1;
        let bytecode_len = raw_bytes.len();
        let pattern = identify_pattern(raw_bytes);

        if rank < top_n {
            // Detailed output for top 10
            println!(
                "━━━ #{} | skeleton {:?} | {} copies | bytecodeLen={}B | skeletonLen={}B ━━━",
                rank + 1,
                &skel_hash_hex,
                count,
                bytecode_len,
                group.skeleton_len,
            );
            println!("  Pattern:  {pattern}");

            // First 64 bytes hex
            let prefix_len = raw_bytes.len().min(64);
            println!(
                "  First {}B: {}",
                prefix_len,
                hex::encode(&raw_bytes[..prefix_len])
            );

            // Known selectors
            let known = find_selectors_in_bytecode(raw_bytes);
            if !known.is_empty() {
                println!("  Known selectors:");
                for name in &known {
                    println!("    - {name}");
                }
            } else {
                // Show raw PUSH4 selectors
                let all_push4 = find_all_push4_selectors(raw_bytes);
                if !all_push4.is_empty() {
                    let show = all_push4.len().min(12);
                    println!(
                        "  PUSH4 selectors ({} total): {}{}",
                        all_push4.len(),
                        all_push4[..show].join(" "),
                        if all_push4.len() > show { " ..." } else { "" }
                    );
                } else {
                    println!("  No PUSH4 selectors found");
                }
            }

            // EIP-1167: show implementation address
            if let Some(impl_addr) = eip1167_impl_addr(raw_bytes) {
                println!("  EIP-1167 impl address: {impl_addr:?}");
                // Check if all members point to the same impl
                let all_same = group.members.iter().all(|(_, b)| {
                    eip1167_impl_addr(b) == Some(impl_addr)
                });
                if all_same {
                    println!("  (all {count} copies point to same implementation)");
                } else {
                    // Count distinct implementations
                    let mut impl_addrs: HashMap<Address, usize> = HashMap::new();
                    for (_, b) in &group.members {
                        if let Some(a) = eip1167_impl_addr(b) {
                            *impl_addrs.entry(a).or_default() += 1;
                        }
                    }
                    println!(
                        "  ({} distinct implementation addresses across {count} proxies)",
                        impl_addrs.len()
                    );
                    // Note: same skeleton means the opcode structure is identical.
                    // For EIP-1167 the PUSH20 immediate (impl address) is stripped,
                    // so ALL EIP-1167 proxies share the same skeleton.
                }
            }

            // Bytecode size range across members
            let sizes: Vec<usize> = group.members.iter().map(|(_, b)| b.len()).collect();
            let min_size = sizes.iter().copied().min().unwrap();
            let max_size = sizes.iter().copied().max().unwrap();
            if min_size != max_size {
                println!("  Bytecode size range: {min_size}B - {max_size}B");
            }

            // Addresses using this skeleton (from the loaded block)
            let mut total_addrs = 0usize;
            let mut example_addrs: Vec<Address> = Vec::new();
            for (ch, _) in &group.members {
                if let Some(addrs) = code_hash_to_addrs.get(ch) {
                    total_addrs += addrs.len();
                    for a in addrs {
                        if example_addrs.len() < 5 {
                            example_addrs.push(*a);
                        }
                    }
                }
            }
            println!(
                "  Addresses in block: {total_addrs} (across {count} code hashes)"
            );
            for addr in &example_addrs {
                println!("    {addr:?}");
            }

            // Representative code_hash
            println!(
                "  Representative code_hash: 0x{}",
                hex::encode(representative.0.as_slice())
            );

            // Check if members have different patterns (unlikely but informative)
            if count <= 5 {
                for (idx, (ch, b)) in group.members.iter().enumerate() {
                    let p = identify_pattern(b);
                    println!(
                        "    member[{idx}]: {}B  code_hash=0x{}..  pattern={p}",
                        b.len(),
                        &hex::encode(ch.as_slice())[..12]
                    );
                }
            }
            println!();
        } else {
            // Compact output for remaining groups
            println!(
                "  #{:>3} | skel={} | copies={:>4} | {}B | {pattern}",
                rank + 1,
                &skel_hash_hex[..12],
                count,
                bytecode_len,
            );
        }
    }

    // Summary table
    println!("\n=== Summary: all duplicated skeletons ===");
    println!(
        "{:>5} {:>6} {:>8} {:>8}  {}",
        "rank", "copies", "byteLen", "skelLen", "pattern"
    );
    for (rank, group) in sorted_groups.iter().enumerate() {
        if group.members.len() < 2 {
            break;
        }
        let raw = &group.members[0].1;
        let pattern = identify_pattern(raw);
        println!(
            "{:>5} {:>6} {:>7}B {:>7}B  {}",
            rank + 1,
            group.members.len(),
            raw.len(),
            group.skeleton_len,
            pattern,
        );
    }
}
