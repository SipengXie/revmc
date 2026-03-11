# PGO Branch Profiling for Skeleton JIT

> **For Claude:** Execute this plan using the skill chosen during Execution Handoff (see end of plan).
> Planning dir: .planning/

**Goal:** Add profile-guided branch weights to skeleton JIT compilation and benchmark L1i cache improvement.

**Architecture:** Three-phase approach: (1) collect real JUMPI branch profiles by running a block through the interpreter with an `Inspector` that records taken/not-taken counts, (2) thread profile data into the skeleton compiler so JUMPI emits `brif_cold` with real weights, (3) benchmark skeleton+PGO vs skeleton-only vs native on block 38004930.

**Tech Stack:** revmc (LLVM backend), revm interpreter, existing skeleton_bench harness, perf stat for L1i counters.

---

### Task 1: Define BranchProfile data type

**Files:**
- Create: `crates/revmc/src/profile.rs`
- Modify: `crates/revmc/src/lib.rs` (add `pub mod profile;`)

**Step 1: Write the profile data structure**

```rust
// crates/revmc/src/profile.rs
//! Branch profile data for PGO-guided compilation.

use std::collections::HashMap;

/// Branch profile for a single bytecode (keyed by bytecode PC).
/// Each entry records how many times a JUMPI was taken vs not-taken.
#[derive(Clone, Debug, Default)]
pub struct BranchProfile {
    /// Map from bytecode PC → (taken_count, not_taken_count).
    pub branches: HashMap<u32, (u64, u64)>,
}

impl BranchProfile {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a branch outcome at the given PC.
    pub fn record(&mut self, pc: u32, taken: bool) {
        let entry = self.branches.entry(pc).or_insert((0, 0));
        if taken {
            entry.0 += 1;
        } else {
            entry.1 += 1;
        }
    }

    /// Returns true if the taken direction is cold (< 20% of total).
    /// Returns None if no profile data for this PC.
    pub fn is_taken_cold(&self, pc: u32) -> Option<bool> {
        self.branches.get(&pc).map(|&(taken, not_taken)| {
            let total = taken + not_taken;
            if total == 0 { return false; }
            // "cold" = less than 20% of total executions
            taken * 5 < total
        })
    }

    /// Returns true if the not-taken direction is cold (< 20% of total).
    pub fn is_not_taken_cold(&self, pc: u32) -> Option<bool> {
        self.branches.get(&pc).map(|&(taken, not_taken)| {
            let total = taken + not_taken;
            if total == 0 { return false; }
            not_taken * 5 < total
        })
    }
}
```

**Step 2: Register the module**

In `crates/revmc/src/lib.rs`, add:
```rust
pub mod profile;
```

**Step 3: Run existing tests to verify no breakage**

Run: `cargo test -p revmc --lib 2>&1 | tail -5`
Expected: All existing tests pass.

**Step 4: Commit**

```bash
git add crates/revmc/src/profile.rs crates/revmc/src/lib.rs
git commit -m "feat(pgo): add BranchProfile data type for JUMPI profiling"
```

> **Note:** Log discoveries to `.planning/findings.md` after this task.

---

### Task 2: Thread profile data into the compiler

**Files:**
- Modify: `crates/revmc/src/compiler/translate.rs:18-27` (FcxConfig)
- Modify: `crates/revmc/src/compiler/mod.rs:220-252` (translate/translate_skeleton)
- Modify: `crates/revmc/src/compiler/mod.rs:42-59` (EvmCompiler struct)

**Step 1: Add optional profile to FcxConfig**

In `crates/revmc/src/compiler/translate.rs`, add a field to `FcxConfig`:

```rust
pub(super) struct FcxConfig {
    pub(super) comments: bool,
    pub(super) debug_assertions: bool,
    pub(super) frame_pointers: bool,

    pub(super) local_stack: bool,
    pub(super) inspect_stack_length: bool,
    pub(super) stack_bound_checks: bool,
    pub(super) gas_metering: bool,

    /// Optional branch profile for PGO-guided compilation.
    pub(super) branch_profile: Option<crate::profile::BranchProfile>,
}
```

Update `Default for FcxConfig` to include `branch_profile: None`.

Note: `FcxConfig` currently derives `Clone, Copy` — `BranchProfile` contains a `HashMap` which is not `Copy`. Change `#[derive(Clone, Copy, Debug)]` to `#[derive(Clone, Debug)]` on FcxConfig. Then find all places that require `Copy` for FcxConfig and fix them (likely none since it's passed by reference in `translate_inner` and `make_builder`).

**Step 2: Add public API on EvmCompiler**

In `crates/revmc/src/compiler/mod.rs`, add a method:

```rust
/// Sets the branch profile for PGO-guided compilation.
/// When set, JUMPI instructions will emit branch weight hints based on
/// the provided taken/not-taken counts.
pub fn set_branch_profile(&mut self, profile: crate::profile::BranchProfile) {
    self.config.branch_profile = Some(profile);
}

/// Clears the branch profile.
pub fn clear_branch_profile(&mut self) {
    self.config.branch_profile = None;
}
```

**Step 3: Run existing tests**

Run: `cargo test -p revmc --lib 2>&1 | tail -5`
Expected: All existing tests pass (no behavioral change yet).

**Step 4: Commit**

```bash
git add crates/revmc/src/compiler/translate.rs crates/revmc/src/compiler/mod.rs
git commit -m "feat(pgo): thread BranchProfile through FcxConfig to compiler"
```

> **Note:** Log discoveries to `.planning/findings.md` after this task.

---

### Task 3: Emit branch weights on JUMPI

**Files:**
- Modify: `crates/revmc/src/compiler/translate.rs:929-936` (JUMPI translation)

**Step 1: Write a unit test for PGO JUMPI**

In `crates/revmc/src/profile.rs`, add:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cold_detection() {
        let mut p = BranchProfile::new();
        // 100 taken, 5 not-taken → not-taken is cold
        p.record(10, true);
        for _ in 0..99 { p.record(10, true); }
        for _ in 0..5 { p.record(10, false); }

        assert_eq!(p.is_taken_cold(10), Some(false));
        assert_eq!(p.is_not_taken_cold(10), Some(true));

        // No data for PC 99
        assert_eq!(p.is_taken_cold(99), None);
    }

    #[test]
    fn test_balanced_not_cold() {
        let mut p = BranchProfile::new();
        for _ in 0..50 { p.record(20, true); }
        for _ in 0..50 { p.record(20, false); }

        // 50/50 — neither is cold
        assert_eq!(p.is_taken_cold(20), Some(false));
        assert_eq!(p.is_not_taken_cold(20), Some(false));
    }
}
```

**Step 2: Run the test**

Run: `cargo test -p revmc --lib profile 2>&1 | tail -5`
Expected: PASS

**Step 3: Modify JUMPI translation**

In `crates/revmc/src/compiler/translate.rs`, change the JUMPI branch (around line 929-936):

```rust
// Current code:
//     if opcode == op::JUMPI {
//         let cond_word = self.pop();
//         let cond = self.bcx.icmp_imm(IntCC::NotEqual, cond_word, 0);
//         let next = self.inst_entries[inst + 1];
//         if target == self.return_block.unwrap() {
//             self.add_invalid_jump();
//         }
//         self.bcx.brif(cond, target, next);
//     }

// New code:
if opcode == op::JUMPI {
    let cond_word = self.pop();
    let cond = self.bcx.icmp_imm(IntCC::NotEqual, cond_word, 0);
    let next = self.inst_entries[inst + 1];
    if target == self.return_block.unwrap() {
        self.add_invalid_jump();
    }
    // PGO: if we have branch profile, emit weighted branch.
    // cond=true → jump to target (taken), cond=false → fall through (not-taken).
    let used_pgo = if let Some(ref profile) = self.config.branch_profile {
        let pc = data.pc;
        if let Some(&(taken, not_taken)) = profile.branches.get(&pc) {
            let total = taken + not_taken;
            if total > 0 && (taken * 5 < total || not_taken * 5 < total) {
                // One side is <20% → use brif_cold
                let then_is_cold = taken * 5 < total;
                self.bcx.brif_cold(cond, target, next, then_is_cold);
                true
            } else {
                false
            }
        } else {
            false
        }
    } else {
        false
    };
    if !used_pgo {
        self.bcx.brif(cond, target, next);
    }
}
```

Note: `self.config` is `FcxConfig` which is stored in `FunctionCx`. The `data.pc` field is `u32` — matches the key type in `BranchProfile.branches`.

**Step 4: Run all tests**

Run: `cargo test -p revmc 2>&1 | tail -5`
Expected: All existing tests pass (no profile set → falls through to `brif`).

**Step 5: Commit**

```bash
git add crates/revmc/src/compiler/translate.rs crates/revmc/src/profile.rs
git commit -m "feat(pgo): emit branch weights on JUMPI when profile data available"
```

> **Note:** Log discoveries to `.planning/findings.md` after this task.

---

### Task 4: Build profile collector binary (Inspector-based)

**Files:**
- Create: `examples/runner/src/bin/collect_profile.rs`

This binary runs all transactions in a block through the interpreter with a custom `Inspector`,
recording real JUMPI branch outcomes (taken/not-taken) per contract.

**Step 1: Implement BranchCollector Inspector**

The `Inspector::step()` callback fires before each opcode. For JUMPI (0x57):
- `interp.bytecode.pc()` → current PC
- `interp.stack.data()[len-2]` → condition value (second from top; top is jump destination)
- `condition != 0` → taken, `== 0` → not-taken
- `interp.bytecode.get_or_calculate_hash()` → code hash (key for per-contract profile)

No need for `step_end()` — the condition value alone determines the branch direction.

```rust
//! Collect JUMPI branch profiles by running a block with an Inspector.
//!
//! Output: /tmp/branch_profile_{block}.bin (serialized HashMap<B256, BranchProfile>)
//!
//! Usage:
//!   cargo run -p revmc-examples-runner --bin collect_profile --release [bench_dir] [block]

#[allow(dead_code)]
#[path = "../bin_common.rs"]
mod bin_common;

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use revm::bytecode::opcode as op;
use revm::inspector::Inspector;
use revm::interpreter::{Interpreter, InterpreterTypes};
use revm::primitives::{B256, U256};

use bin_common::{build_op_cfg, build_op_tx, BinLoader, OpCtx};

use revmc::profile::BranchProfile;

/// Collected profiles: code_hash → BranchProfile.
type ProfileMap = HashMap<B256, BranchProfile>;

/// Inspector that records JUMPI taken/not-taken counts.
#[derive(Default)]
struct BranchCollector {
    profiles: ProfileMap,
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for BranchCollector {
    fn step(&mut self, interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        if interp.bytecode.opcode() == op::JUMPI {
            let pc = interp.bytecode.pc() as u32;
            let stack_data = interp.stack.data();
            if stack_data.len() >= 2 {
                // JUMPI stack: [..., destination, condition]
                // condition is second from top (stack grows upward)
                let condition = stack_data[stack_data.len() - 2];
                let taken = condition != U256::ZERO;
                let hash = interp.bytecode.get_or_calculate_hash();
                self.profiles.entry(hash).or_default().record(pc, taken);
            }
        }
    }
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

    println!("=== Branch Profile Collector (Inspector) ===");
    println!("Block: {block}\n");

    let loader = BinLoader::new(bench_dir, block).unwrap();
    println!(
        "Loaded {} accounts, {} codes, {} txs",
        loader.account_count(), loader.code_count(), loader.tx_count()
    );

    let t = Instant::now();

    // Build EVM with BranchCollector inspector, run all txs in the block.
    let mut collector = BranchCollector::default();
    // ... (build EVM with inspector, iterate over txs, execute each)
    // Use `ctx.build_mainnet_with_inspector(collector)` pattern.
    // The exact wiring depends on bin_common's make_evm / NativeHandler API.
    // Key: run every tx through the interpreter so step() fires on each opcode.

    let dur = t.elapsed();
    let profiles = collector.profiles;

    println!(
        "\nCollected profiles for {} contracts in {:.3}s",
        profiles.len(), dur.as_secs_f64()
    );

    // Stats
    let total_jumpis: usize = profiles.values().map(|p| p.branches.len()).sum();
    let total_samples: u64 = profiles.values()
        .flat_map(|p| p.branches.values())
        .map(|(t, nt)| t + nt)
        .sum();
    println!("  Total JUMPI positions profiled: {total_jumpis}");
    println!("  Total branch samples: {total_samples}");

    // Cold branch stats
    let cold_count: usize = profiles.values()
        .flat_map(|p| p.branches.iter())
        .filter(|(_, &(t, nt))| {
            let total = t + nt;
            total > 0 && (t * 5 < total || nt * 5 < total)
        })
        .count();
    println!("  JUMPI with cold branch (<20%): {cold_count} ({:.1}%)",
        cold_count as f64 / total_jumpis.max(1) as f64 * 100.0);

    // Save as bincode
    let out = format!("/tmp/branch_profile_{block}.bin");
    let data = bincode::serialize(&profiles).unwrap();
    std::fs::write(&out, &data).unwrap();
    println!("  Saved to: {out} ({} bytes)", data.len());
}
```

**Why Inspector instead of static heuristic:**
- Gets **real runtime** taken/not-taken counts from actual transaction execution
- Correctly handles loops (backward JUMPI where taken = hot) vs require checks (forward JUMPI where taken = cold)
- Only profiles JUMPI positions that are actually executed (dead code is ignored)
- Costs one extra block execution (~20ms) — negligible compared to compilation time

**Step 2: Build and run**

Run: `cargo run -p revmc-examples-runner --bin collect_profile --release 2>&1`
Expected: Outputs profile file at `/tmp/branch_profile_38004930.bin` with real branch counts.

**Step 3: Commit**

```bash
git add examples/runner/src/bin/collect_profile.rs
git commit -m "feat(pgo): add branch profile collector (Inspector-based runtime profiling)"
```

> **Note:** Log discoveries to `.planning/findings.md` after this task.

---

### Task 5: Build PGO benchmark binary

**Files:**
- Create: `examples/runner/src/bin/pgo_bench.rs`

This is the core benchmark: compares skeleton (no PGO) vs skeleton+PGO vs native.

**Step 1: Write the benchmark**

The benchmark follows the same pattern as `skeleton_bench.rs` but adds a third mode: skeleton+PGO.

Key differences from skeleton_bench:
1. Load branch profiles from `/tmp/branch_profile_{block}.bin`
2. Compile skeleton groups twice: once without profile, once with profile
3. Run 3 modes: native, skeleton, skeleton+PGO
4. Report timing + L1i stats

The binary should:
1. Load block data (BinLoader)
2. Load branch profiles
3. Group bytecodes by skeleton
4. Compile three sets of functions:
   - Per-hash (baseline, from AOT cache)
   - Skeleton (from AOT cache, no PGO)
   - Skeleton+PGO (fresh compile with branch weights, save to separate cache dir `/tmp/jit_cache_pgo/`)
5. Run 5 warmup + 30 measurement rounds for each mode
6. Report median times and speedup ratios

Implementation: Base this heavily on `skeleton_bench.rs`. The main addition is loading profiles, calling `compiler.set_branch_profile(profile)` before compiling skeleton groups, and comparing the third mode.

This file will be ~400-500 lines. The implementer should copy the skeleton_bench pattern and add the PGO compilation path.

**Step 2: Build and run**

Run: `cargo build -p revmc-examples-runner --bin pgo_bench --release 2>&1 | tail -3`
Run: `cargo run -p revmc-examples-runner --bin pgo_bench --release 2>&1`

Expected output format:
```
=== PGO Branch Profiling Benchmark ===
Block 38004930

Mode               | Median (ms) | vs Native
Native             |     21.1    |   1.00x
Skeleton (no PGO)  |     16.5    |   1.28x
Skeleton + PGO     |     ?.??    |   ?.??x
```

**Step 3: Commit**

```bash
git add examples/runner/src/bin/pgo_bench.rs
git commit -m "feat(pgo): add PGO benchmark (skeleton vs skeleton+PGO vs native)"
```

> **Note:** Log discoveries to `.planning/findings.md` after this task.

---

### Task 6: Run benchmark and analyze results

**Files:**
- Modify: `.planning/findings.md` (record results)

**Step 1: Run the profile collector**

```bash
cargo run -p revmc-examples-runner --bin collect_profile --release
```

**Step 2: Run the PGO benchmark**

```bash
cargo run -p revmc-examples-runner --bin pgo_bench --release
```

**Step 3: Run with perf stat for L1i counters**

```bash
perf stat -e L1-icache-load-misses,iTLB-load-misses,branch-misses,instructions \
  cargo run -p revmc-examples-runner --bin pgo_bench --release -- \
  /home/ubuntu/sipeng/bench_data 38004930 2>&1
```

**Step 4: Record findings**

Update `.planning/findings.md` with:
- Median execution times for all 3 modes
- L1i miss counts (if perf stat available)
- Whether PGO provides measurable improvement
- Analysis of why/why not

**Step 5: Commit findings**

```bash
git add .planning/findings.md
git commit -m "docs(pgo): record PGO branch profiling benchmark results"
```

> **Note:** Log discoveries to `.planning/findings.md` after this task.

---

### Parallelism Groups

- **Group A** (sequential): Task 1 → Task 2 → Task 3
  - Task 1: BranchProfile data type (compiler crate)
  - Task 2: thread profile into compiler (depends on Task 1 for type)
  - Task 3: emit branch weights (depends on Task 2 for plumbing)
- **Group B** (after Task 1): Task 4
  - Task 4: Inspector-based profile collector binary (depends on Task 1 for BranchProfile type)
  - Can run in parallel with Tasks 2-3 (only needs the type from Task 1)
- **Group C** (after Group A + Group B): Task 5
  - Needs both compiler changes (Task 3) and profile collector (Task 4)
- **Group D** (after Group C): Task 6
  - Run and analyze

**Parallelism score:** After Task 1, Tasks 2-3 and Task 4 can run in parallel

---

### Execution Handoff

This plan has light serial dependencies and medium complexity. Since Tasks 2→3 are tightly coupled edits to the same files, sequential execution is safest. After Task 1 completes, Task 4 (Inspector collector) can run in parallel with Tasks 2-3 (compiler plumbing).
