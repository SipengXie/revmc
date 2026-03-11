//! Collect JUMPI branch profiles by running block transactions with an Inspector.
//!
//! Usage:
//!   cargo run -p revmc-examples-runner --bin collect_profile --release [bench_dir] [block]
//!
//! Defaults: bench_dir = /home/ubuntu/sipeng/bench_data, block = 38004930

#[allow(dead_code)]
#[path = "../bin_common.rs"]
mod bin_common;

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use op_revm::transaction::OpTransaction;
use op_revm::DefaultOp;
use revm::database::EmptyDB;
use revm::interpreter::interpreter::EthInterpreter;
use revm::interpreter::interpreter_types::Jumps;
use revm::interpreter::Interpreter;
use revm::primitives::B256;
use revm::inspector::InspectEvm;
use revm::Inspector;
use revmc::profile::BranchProfile;
use revmc_builtins as _;
use serde::{Deserialize, Serialize};

use bin_common::{build_op_cfg, build_op_tx, BinLoader, OpCtx};

// JUMPI opcode
const JUMPI: u8 = 0x57;

// ── BranchCollector Inspector ────────────────────────────────────────────────

/// Collects JUMPI branch outcomes per contract (keyed by bytecode hash).
struct BranchCollector {
    /// Per-contract branch profiles: bytecode_hash → BranchProfile.
    profiles: HashMap<B256, BranchProfile>,
    /// Current bytecode hash (set in initialize_interp, used in step).
    current_hash: B256,
}

impl BranchCollector {
    fn new() -> Self {
        Self {
            profiles: HashMap::new(),
            current_hash: B256::ZERO,
        }
    }
}

impl<CTX> Inspector<CTX, EthInterpreter> for BranchCollector {
    fn initialize_interp(&mut self, interp: &mut Interpreter<EthInterpreter>, _context: &mut CTX) {
        // Cache the bytecode hash for subsequent step() calls in this frame.
        self.current_hash = interp.bytecode.get_or_calculate_hash();
    }

    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _context: &mut CTX) {
        let opcode = interp.bytecode.opcode();
        if opcode != JUMPI {
            return;
        }

        let pc = interp.bytecode.pc() as u32;

        // JUMPI stack layout (before execution):
        //   top:     jump destination
        //   top - 1: condition
        // stack.data() is bottom-to-top, so:
        //   data[len - 1] = top (destination)
        //   data[len - 2] = condition
        let data = interp.stack.data();
        let len = data.len();
        if len < 2 {
            return; // stack underflow, interpreter will handle the error
        }
        let condition = data[len - 2];
        let taken = !condition.is_zero();

        self.profiles
            .entry(self.current_hash)
            .or_default()
            .record(pc, taken);
    }
}

// ── Serializable profile wrapper ─────────────────────────────────────────────

/// Serializable wrapper for branch profiles (BranchProfile lacks serde derives).
#[derive(Serialize, Deserialize)]
struct SerializableProfiles {
    /// Map from bytecode hash bytes → (pc → (taken, not_taken)).
    profiles: HashMap<[u8; 32], HashMap<u32, (u64, u64)>>,
}

impl SerializableProfiles {
    fn from_profiles(profiles: &HashMap<B256, BranchProfile>) -> Self {
        let mut map = HashMap::with_capacity(profiles.len());
        for (hash, profile) in profiles {
            map.insert(hash.0, profile.branches.clone());
        }
        Self { profiles: map }
    }
}

// ── Main ─────────────────────────────────────────────────────────────────────

fn main() {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/home/ubuntu/sipeng/bench_data".into());
    let bench_dir = Path::new(&dir);
    let block: u64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(38004930);

    println!("=== Branch Profile Collector ===");
    println!("Block {block}\n");

    // Load block data
    let loader = BinLoader::new(bench_dir, block).unwrap_or_else(|e| {
        eprintln!("Failed: {e}");
        std::process::exit(1);
    });
    println!(
        "Loaded {} accounts, {} codes, {} txs",
        loader.account_count(),
        loader.code_count(),
        loader.tx_count()
    );

    // Build EVM with BranchCollector inspector
    let cfg = build_op_cfg(loader.raw_txs().first().and_then(|tx| tx.chain_id));
    let db = loader.build_cache_db();
    let dummy_tx = OpTransaction::builder().build_fill();
    let ctx = OpCtx::<EmptyDB>::op()
        .with_cfg(cfg)
        .with_db(db)
        .with_tx(dummy_tx);
    let collector = BranchCollector::new();
    let mut evm = op_revm::OpEvm::new(ctx, collector);

    // Execute all transactions with the inspector
    let t0 = Instant::now();
    let mut tx_ok = 0u32;
    let mut tx_skip = 0u32;
    let mut tx_fail = 0u32;

    for tx_bin in loader.raw_txs() {
        // Skip deposit transactions
        if tx_bin.tx_type == 0x7e {
            tx_skip += 1;
            continue;
        }
        let op_tx = build_op_tx(tx_bin);
        match evm.inspect_one_tx(op_tx) {
            Ok(_) => {
                // State is committed to the journal internally by inspect_one_tx.
                // Do NOT call finalize() here — it would clear the journal state.
                tx_ok += 1;
            }
            Err(e) => {
                eprintln!("  tx error: {e:?}");
                tx_fail += 1;
            }
        }
    }
    let elapsed = t0.elapsed();

    // Extract profiles from the inspector
    let profiles = &evm.0.inspector.profiles;

    // Compute statistics
    let num_contracts = profiles.len();
    let total_positions: usize = profiles.values().map(|p| p.branches.len()).sum();
    let total_samples: u64 = profiles
        .values()
        .flat_map(|p| p.branches.values())
        .map(|(t, nt)| t + nt)
        .sum();

    // Count cold branches (< 20% of total for that position)
    let mut cold_positions = 0u32;
    let mut total_branch_positions = 0u32;
    for profile in profiles.values() {
        for &(taken, not_taken) in profile.branches.values() {
            let total = taken + not_taken;
            if total == 0 {
                continue;
            }
            total_branch_positions += 1;
            // A position has a "cold" direction if one side is < 20%
            let has_cold = (taken * 5 < total) || (not_taken * 5 < total);
            if has_cold {
                cold_positions += 1;
            }
        }
    }
    let cold_pct = if total_branch_positions > 0 {
        cold_positions as f64 / total_branch_positions as f64 * 100.0
    } else {
        0.0
    };

    println!("\n=== Profile Statistics ===");
    println!("  Execution time: {:.3}s", elapsed.as_secs_f64());
    println!(
        "  Transactions: {} ok, {} skipped (deposit), {} failed",
        tx_ok, tx_skip, tx_fail
    );
    println!("  Contracts profiled: {num_contracts}");
    println!("  Total JUMPI positions: {total_positions}");
    println!("  Total samples: {total_samples}");
    println!(
        "  Cold branch positions: {cold_positions}/{total_branch_positions} ({cold_pct:.1}%)"
    );

    // Save profiles
    let output_path = format!("/tmp/branch_profile_{block}.bin");
    let serializable = SerializableProfiles::from_profiles(profiles);
    let data = bincode::serialize(&serializable).expect("serialize profiles");
    std::fs::write(&output_path, &data).expect("write profile file");
    println!(
        "\nSaved profile to {output_path} ({:.1} KB)",
        data.len() as f64 / 1024.0
    );

    println!("\nDone.");
}
