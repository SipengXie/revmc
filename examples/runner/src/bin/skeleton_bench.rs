//! Skeleton-aware compilation benchmark.
//!
//! Verifies the skeleton compilation pipeline works end-to-end by:
//! 1. Loading block state data, grouping contracts by opcode skeleton
//! 2. Finding the largest skeleton group (most duplicated)
//! 3. Analyzing PUSH variance across that group
//! 4. Compiling one contract two ways: per-hash `jit()` vs skeleton-aware `jit_skeleton()`
//! 5. Printing compilation times and variance stats
//!
//! Usage:
//!   cargo run -p revmc-examples-runner --bin skeleton_bench --release [bench_dir] [block]

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use revm::bytecode::Bytecode;
use revm::primitives::{Address, B256, HashMap as RevmHashMap, U256};
use revm::state::AccountInfo;
use revmc::skeleton::{analyze_skeleton_group, build_data_table, PushClassification};
use revmc::{EvmCompiler, EvmLlvmBackend, OptimizationLevel};
use revm::primitives::hardfork::SpecId;
use serde::Deserialize;

// Standalone snapshot structs (same as opcode_dedup.rs / skeleton_export.rs)
#[derive(Deserialize)]
struct AccountSnapshot {
    #[allow(dead_code)]
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
    #[allow(dead_code)]
    accounts: RevmHashMap<Address, AccountSnapshot>,
    codes: RevmHashMap<B256, Bytecode>,
}

/// Extract opcode skeleton by stripping PUSH1..PUSH32 immediates.
fn extract_opcode_skeleton(bytes: &[u8]) -> Vec<u8> {
    let mut skeleton = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let op = bytes[i];
        skeleton.push(op);
        i += 1;
        if op >= 0x60 && op <= 0x7f {
            i += (op - 0x5f) as usize;
        }
    }
    skeleton
}

/// Hash a byte slice for skeleton grouping.
fn hash_bytes(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn main() {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/home/ubuntu/sipeng/bench_data".into());
    let bench_dir = Path::new(&dir);
    let block: u64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(38004930);

    println!("=== Skeleton-Aware Compilation Benchmark ===");
    println!("Loading block {block} from {dir}\n");

    // Phase 1: load state snapshot
    let path = bench_dir.join(format!("states/{block}.bin"));
    let data = std::fs::read(&path).unwrap_or_else(|e| {
        eprintln!("Failed to read {}: {e}", path.display());
        std::process::exit(1);
    });
    let snapshot: CacheSnapshot = bincode::deserialize(&data).unwrap_or_else(|e| {
        eprintln!("Failed to deserialize {}: {e}", path.display());
        std::process::exit(1);
    });

    let all_codes: HashMap<B256, Vec<u8>> = snapshot
        .codes
        .iter()
        .filter(|(_, bc)| !bc.is_empty())
        .map(|(h, bc)| (*h, bc.original_byte_slice().to_vec()))
        .collect();
    println!("Loaded {} non-empty bytecodes from block {block}", all_codes.len());

    // Phase 2: group by opcode skeleton
    let mut skeleton_groups: HashMap<u64, Vec<(B256, Vec<u8>)>> = HashMap::new();
    for (hash, bytes) in &all_codes {
        let skel = extract_opcode_skeleton(bytes);
        let skel_hash = hash_bytes(&skel);
        skeleton_groups
            .entry(skel_hash)
            .or_default()
            .push((*hash, bytes.clone()));
    }

    // Find largest group with 2+ members
    let mut dup_groups: Vec<_> = skeleton_groups
        .into_iter()
        .filter(|(_, members)| members.len() >= 2)
        .collect();
    dup_groups.sort_by_key(|(_, m)| std::cmp::Reverse(m.len()));

    println!(
        "Skeleton groups: {} total, {} with 2+ members",
        all_codes.len(),
        dup_groups.len()
    );

    if dup_groups.is_empty() {
        println!("\nNo duplicated skeletons found in this block. Try scanning more blocks.");
        return;
    }

    let (skel_hash, ref members) = dup_groups[0];
    let group_size = members.len();
    let bytecode_len = members[0].1.len();
    println!(
        "\nLargest skeleton group: {:016x} ({} copies, {}B bytecode)",
        skel_hash, group_size, bytecode_len
    );

    // Phase 3: analyze variance using revmc::skeleton API
    let bytecodes_refs: Vec<&[u8]> = members.iter().map(|(_, b)| b.as_slice()).collect();
    let variance = analyze_skeleton_group(&bytecodes_refs);

    let invariant_count = variance
        .pushes
        .iter()
        .filter(|p| **p == PushClassification::Invariant)
        .count();
    let variant_count = variance.num_variant as usize;

    println!("\nVariance analysis:");
    println!("  Total PUSH1..PUSH32 positions: {}", variance.pushes.len());
    println!("  Invariant (compiled as constants): {invariant_count}");
    println!("  Variant (loaded from data table):  {variant_count}");
    if !variance.pushes.is_empty() {
        println!(
            "  Variant fraction: {:.1}%",
            variant_count as f64 / variance.pushes.len() as f64 * 100.0
        );
    }

    // Build data table for the first instance
    let data_table = build_data_table(&members[0].1, &variance);
    println!(
        "  Data table size: {} bytes ({} variant slots x 32 bytes)",
        data_table.data.len(),
        variance.num_variant
    );

    // Phase 4: compile one contract two ways
    let target_hash = members[0].0;
    let target_bytes = &members[0].1;
    let spec = SpecId::PRAGUE;

    println!(
        "\nCompiling contract {} ({}B)...",
        &hex::encode(target_hash)[..16],
        target_bytes.len()
    );

    // Method 1: standard per-hash JIT
    let t1 = Instant::now();
    let per_hash_ok = {
        let context = Box::leak(Box::new(revmc::llvm::inkwell::context::Context::create()));
        let backend = EvmLlvmBackend::new(context, false, OptimizationLevel::Aggressive)
            .expect("LLVM backend");
        let compiler: &'static mut EvmCompiler<EvmLlvmBackend<'static>> =
            Box::leak(Box::new(EvmCompiler::new(backend)));
        let name = format!("c_{}", &hex::encode(target_hash)[..16]);
        match unsafe { compiler.jit(&name, target_bytes.as_slice(), spec) } {
            Ok(_fn_ptr) => true,
            Err(e) => {
                eprintln!("  per-hash jit() failed: {e}");
                false
            }
        }
    };
    let per_hash_elapsed = t1.elapsed();

    // Method 2: skeleton-aware JIT
    let t2 = Instant::now();
    let skeleton_ok = {
        let context = Box::leak(Box::new(revmc::llvm::inkwell::context::Context::create()));
        let backend = EvmLlvmBackend::new(context, false, OptimizationLevel::Aggressive)
            .expect("LLVM backend");
        let compiler: &'static mut EvmCompiler<EvmLlvmBackend<'static>> =
            Box::leak(Box::new(EvmCompiler::new(backend)));
        let name = format!("s_{}", &hex::encode(target_hash)[..16]);
        match unsafe {
            compiler.jit_skeleton(&name, target_bytes.as_slice(), spec, &variance)
        } {
            Ok(_fn_ptr) => true,
            Err(e) => {
                eprintln!("  skeleton jit_skeleton() failed: {e}");
                false
            }
        }
    };
    let skeleton_elapsed = t2.elapsed();

    // Results
    println!("\n=== Compilation Results ===");
    println!(
        "  per-hash jit():      {} in {:.3}s",
        if per_hash_ok { "OK" } else { "FAIL" },
        per_hash_elapsed.as_secs_f64()
    );
    println!(
        "  skeleton jit_skeleton(): {} in {:.3}s",
        if skeleton_ok { "OK" } else { "FAIL" },
        skeleton_elapsed.as_secs_f64()
    );
    if per_hash_ok && skeleton_ok {
        let ratio = per_hash_elapsed.as_secs_f64() / skeleton_elapsed.as_secs_f64();
        println!("  Time ratio (per-hash / skeleton): {ratio:.2}x");
    }

    // Phase 5: verify data tables for all group members
    if skeleton_ok {
        println!("\n=== Data Table Verification ({group_size} instances) ===");
        for (i, (hash, bytes)) in members.iter().enumerate() {
            let table = build_data_table(bytes, &variance);
            let short_hash = &hex::encode(hash)[..12];
            if i < 5 || i == group_size - 1 {
                println!(
                    "  [{}] {}: {} bytes table OK",
                    i, short_hash, table.data.len()
                );
            } else if i == 5 {
                println!("  ... ({} more) ...", group_size - 6);
            }
        }
        println!("  All {group_size} data tables built successfully.");
    }

    println!("\nDone.");
}
