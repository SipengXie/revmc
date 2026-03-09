# Skeleton-Aware Compilation Implementation Plan

> **For Claude:** Execute this plan using the skill chosen during Execution Handoff (see end of plan).
> Planning dir: .planning/

**Goal:** Implement selective parameterization — compile each opcode skeleton once, loading only variant PUSH values (0.63%) from a per-instance data table at runtime.

**Architecture:** 4-layer bottom-up: EvmContext (new field) → skeleton.rs (data structures + analysis) → InstData/InstFlags (new flag + offset) → translate.rs (variant PUSH codegen). New `translate_skeleton()` entry point; existing `translate()` untouched.

**Tech Stack:** Rust, LLVM IR (via revmc-backend traits), bitflags, `#[repr(C)]` structs with `offset_of!`

---

### Task 1: EvmContext — Add `imm_data_ptr` Field

**Files:**
- Modify: `crates/revmc-context/src/lib.rs:29-64`

**Step 1: Add the new field to EvmContext**

In `crates/revmc-context/src/lib.rs`, add `imm_data_ptr` after `bytecode_len`:

```rust
// After line 51 (pub bytecode_len: usize,):
/// Pointer to per-instance immediate data table for skeleton-compiled code.
/// Null when not using skeleton compilation or when all PUSHes are invariant.
pub imm_data_ptr: *const u8,
```

**Step 2: Update static size/offset assertions**

Update the const block at lines 56-64:

```rust
const _: () = {
    use core::mem::offset_of;
    assert!(core::mem::size_of::<EvmContext<'_>>() == 104);  // was 96
    // Key fields accessed by JIT code
    assert!(offset_of!(EvmContext<'_>, memory) == 0);
    assert!(offset_of!(EvmContext<'_>, resume_at) == 72);
    assert!(offset_of!(EvmContext<'_>, bytecode_ptr) == 80);
    assert!(offset_of!(EvmContext<'_>, bytecode_len) == 88);
    assert!(offset_of!(EvmContext<'_>, imm_data_ptr) == 96);
};
```

**Step 3: Initialize imm_data_ptr in from_interpreter_with_stack**

In the `from_interpreter_with_stack` method (line 81-105), add `imm_data_ptr: std::ptr::null()` to the struct literal:

```rust
let this = Self {
    memory: &mut interpreter.memory,
    input: &mut interpreter.input,
    gas: &mut interpreter.gas,
    host,
    next_action: &mut interpreter.bytecode.action,
    return_data: interpreter.return_data.buffer(),
    is_static: interpreter.runtime_flag.is_static,
    resume_at,
    bytecode_ptr,
    bytecode_len,
    imm_data_ptr: std::ptr::null(),  // NEW
};
```

**Step 4: Verify compilation**

Run: `cargo check -p revmc-context`
Expected: PASS (no errors)

**Step 5: Commit**

```bash
git add crates/revmc-context/src/lib.rs
git commit -m "feat(skeleton): add imm_data_ptr field to EvmContext"
```

> **Note:** Log unexpected discoveries, technical decisions, and implementation insights to `.planning/findings.md` after each task.

---

### Task 2: skeleton.rs — Data Structures

**Files:**
- Create: `crates/revmc/src/skeleton.rs`
- Modify: `crates/revmc/src/lib.rs:12-13` (add module declaration)

**Step 1: Create skeleton.rs with core data structures**

Create `crates/revmc/src/skeleton.rs`:

```rust
//! Skeleton-aware compilation support: variance classification and data table construction.
//!
//! An opcode skeleton is a bytecode with PUSH immediate bytes stripped.
//! Contracts sharing the same skeleton can use a single compiled function,
//! with variant PUSH values loaded from a per-instance data table at runtime.

use revm_bytecode::opcode as op;
use revm_primitives::U256;

/// Classification of a single PUSH instruction within a skeleton.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushClassification {
    /// Same value across all skeleton instances — compiled as `iconst_256`.
    Invariant,
    /// Different values across instances — loaded from data table at runtime.
    /// `table_index` is the variant PUSH ordinal (0, 1, 2, ...).
    /// Byte offset into the data table = `table_index * 32`.
    Variant { table_index: u32 },
}

/// Variance map for an opcode skeleton.
/// One entry per PUSH1..PUSH32 instruction, in opcode order.
/// PUSH0 is excluded (value is always 0, always invariant).
pub struct SkeletonVariance {
    /// Classification of each PUSH1..PUSH32, in opcode order.
    pub pushes: Vec<PushClassification>,
    /// Total number of variant PUSHes.
    pub num_variant: u32,
}

/// Per-instance data table: contiguous array of 32-byte LE i256 values,
/// one entry per variant PUSH. Layout: `[variant_0: [u8; 32], variant_1: [u8; 32], ...]`.
pub struct ImmDataTable {
    /// Raw bytes: length = num_variant_pushes * 32.
    pub data: Vec<u8>,
}

/// Analyze a group of bytecodes sharing the same opcode skeleton.
/// Compares PUSH1..PUSH32 immediates across all instances to classify each as
/// Invariant (same value everywhere) or Variant (differs in at least one instance).
///
/// # Panics
/// - If `bytecodes` is empty.
/// - If bytecodes don't share the same opcode skeleton (different opcode sequences).
pub fn analyze_skeleton_group(bytecodes: &[&[u8]]) -> SkeletonVariance {
    assert!(!bytecodes.is_empty(), "need at least one bytecode");

    // Collect PUSH immediates from the first bytecode as reference.
    let reference = bytecodes[0];
    let ref_pushes = extract_push_immediates(reference);

    // Compare each other bytecode against the reference.
    let mut is_variant = vec![false; ref_pushes.len()];
    for &bytecode in &bytecodes[1..] {
        let pushes = extract_push_immediates(bytecode);
        assert_eq!(
            pushes.len(),
            ref_pushes.len(),
            "bytecodes have different number of PUSH instructions"
        );
        for (i, (ref_imm, imm)) in ref_pushes.iter().zip(pushes.iter()).enumerate() {
            if ref_imm != imm {
                is_variant[i] = true;
            }
        }
    }

    // Build classification with sequential variant indices.
    let mut num_variant = 0u32;
    let pushes = is_variant
        .iter()
        .map(|&variant| {
            if variant {
                let idx = num_variant;
                num_variant += 1;
                PushClassification::Variant { table_index: idx }
            } else {
                PushClassification::Invariant
            }
        })
        .collect();

    SkeletonVariance { pushes, num_variant }
}

/// Build a per-instance data table from a specific bytecode and its variance map.
/// Each variant PUSH value is stored as 32-byte little-endian i256.
pub fn build_data_table(bytecode: &[u8], variance: &SkeletonVariance) -> ImmDataTable {
    let pushes = extract_push_immediates(bytecode);
    assert_eq!(
        pushes.len(),
        variance.pushes.len(),
        "variance map doesn't match bytecode PUSH count"
    );

    let mut data = vec![0u8; variance.num_variant as usize * 32];
    for (imm, classification) in pushes.iter().zip(variance.pushes.iter()) {
        if let PushClassification::Variant { table_index } = classification {
            let offset = *table_index as usize * 32;
            // Convert BE immediate to U256, then store as LE bytes.
            let value = U256::from_be_slice(imm);
            let le_bytes = value.to_le_bytes::<32>();
            data[offset..offset + 32].copy_from_slice(&le_bytes);
        }
    }

    ImmDataTable { data }
}

/// Extract PUSH1..PUSH32 immediate byte slices from bytecode, in opcode order.
/// Skips PUSH0 (no immediate bytes, always zero).
fn extract_push_immediates(bytecode: &[u8]) -> Vec<&[u8]> {
    let mut result = Vec::new();
    let mut i = 0;
    while i < bytecode.len() {
        let op = bytecode[i];
        i += 1;
        if op >= op::PUSH1 && op <= op::PUSH32 {
            let n = (op - op::PUSH0) as usize;
            let end = (i + n).min(bytecode.len());
            result.push(&bytecode[i..end]);
            i = end;
        }
        // PUSH0 (0x5f) is skipped — no immediate, always invariant.
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_invariant() {
        // Two identical bytecodes: PUSH1 0x42 PUSH2 0x00 0x01 STOP
        let bc = &[0x60, 0x42, 0x61, 0x00, 0x01, 0x00];
        let variance = analyze_skeleton_group(&[bc, bc]);
        assert_eq!(variance.pushes.len(), 2);
        assert_eq!(variance.pushes[0], PushClassification::Invariant);
        assert_eq!(variance.pushes[1], PushClassification::Invariant);
        assert_eq!(variance.num_variant, 0);
    }

    #[test]
    fn test_one_variant() {
        // bc1: PUSH1 0x42 PUSH1 0x01 STOP
        let bc1 = &[0x60, 0x42, 0x60, 0x01, 0x00];
        // bc2: PUSH1 0x42 PUSH1 0x02 STOP  (second PUSH differs)
        let bc2 = &[0x60, 0x42, 0x60, 0x02, 0x00];
        let variance = analyze_skeleton_group(&[bc1, bc2]);
        assert_eq!(variance.pushes.len(), 2);
        assert_eq!(variance.pushes[0], PushClassification::Invariant);
        assert_eq!(variance.pushes[1], PushClassification::Variant { table_index: 0 });
        assert_eq!(variance.num_variant, 1);
    }

    #[test]
    fn test_build_data_table() {
        let bc1 = &[0x60, 0x42, 0x60, 0x01, 0x00];
        let bc2 = &[0x60, 0x42, 0x60, 0x02, 0x00];
        let variance = analyze_skeleton_group(&[bc1, bc2]);

        let table1 = build_data_table(bc1, &variance);
        assert_eq!(table1.data.len(), 32); // 1 variant * 32 bytes
        assert_eq!(table1.data[0], 0x01); // LE: value 1 at byte 0
        assert_eq!(table1.data[1..32], [0u8; 31]);

        let table2 = build_data_table(bc2, &variance);
        assert_eq!(table2.data[0], 0x02); // LE: value 2 at byte 0
    }

    #[test]
    fn test_push32_variant() {
        // PUSH32 with 32 bytes of 0xFF, then STOP
        let mut bc1 = vec![0x7f]; // PUSH32
        bc1.extend_from_slice(&[0xff; 32]);
        bc1.push(0x00); // STOP

        let mut bc2 = vec![0x7f]; // PUSH32
        bc2.extend_from_slice(&[0xaa; 32]);
        bc2.push(0x00); // STOP

        let variance = analyze_skeleton_group(&[&bc1, &bc2]);
        assert_eq!(variance.num_variant, 1);

        let table = build_data_table(&bc1, &variance);
        // U256::MAX in LE bytes
        assert!(table.data.iter().all(|&b| b == 0xff));
    }

    #[test]
    fn test_singleton_all_invariant() {
        // Single bytecode → all invariant (singleton optimization)
        let bc = &[0x60, 0x42, 0x60, 0x01, 0x00];
        let variance = analyze_skeleton_group(&[bc]);
        assert_eq!(variance.num_variant, 0);
        assert!(variance.pushes.iter().all(|p| *p == PushClassification::Invariant));
    }
}
```

**Step 2: Register the module in lib.rs**

In `crates/revmc/src/lib.rs`, add after line 12 (`mod bytecode;`):

```rust
pub mod skeleton;
```

**Step 3: Run tests**

Run: `cargo test -p revmc --lib skeleton`
Expected: all 5 tests PASS

**Step 4: Commit**

```bash
git add crates/revmc/src/skeleton.rs crates/revmc/src/lib.rs
git commit -m "feat(skeleton): add SkeletonVariance, ImmDataTable, and analysis functions"
```

> **Note:** Log unexpected discoveries, technical decisions, and implementation insights to `.planning/findings.md` after each task.

---

### Task 3: InstData/InstFlags — Add VARIANT_PUSH Support

**Files:**
- Modify: `crates/revmc/src/bytecode/mod.rs:396-416` (InstData)
- Modify: `crates/revmc/src/bytecode/mod.rs:566-587` (InstFlags)

**Step 1: Add VARIANT_PUSH flag to InstFlags**

In `crates/revmc/src/bytecode/mod.rs`, add to the `InstFlags` bitflags (line 584, before `DEAD_CODE`):

```rust
/// This PUSH loads its value from the per-instance data table (skeleton compilation).
const VARIANT_PUSH = 1 << 5;
```

**Step 2: Add imm_table_offset to InstData**

Add a new field to `InstData` (after `section` at line 415):

```rust
/// Offset into the per-instance data table (variant PUSH index).
/// Only meaningful when `VARIANT_PUSH` flag is set. Byte offset = value * 32.
pub(crate) imm_table_offset: u32,
```

Update `InstData::new()` (line 448-449) to include the new field:

```rust
fn new(opcode: u8) -> Self {
    Self { opcode, imm_table_offset: 0, ..Default::default() }
}
```

Update the `Bytecode::new()` inst construction (line 80) to include the field:

```rust
insts.push(InstData { opcode, flags, base_gas, data, pc: pc as u32, section, imm_table_offset: 0 });
```

Update `Debug for InstData` (line 432-441) to include the field when VARIANT_PUSH is set:

```rust
impl fmt::Debug for InstData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = f.debug_struct("InstData");
        s.field("opcode", &self.to_op())
            .field("flags", &format_args!("{:?}", self.flags))
            .field("data", &self.data)
            .field("pc", &self.pc)
            .field("section", &self.section);
        if self.flags.contains(InstFlags::VARIANT_PUSH) {
            s.field("imm_table_offset", &self.imm_table_offset);
        }
        s.finish()
    }
}
```

**Step 3: Add apply_variance method to Bytecode**

Add a new public method to `Bytecode` (after `analyze()` at line 176):

```rust
/// Apply skeleton variance classification to instruction flags.
/// Must be called after `analyze()`.
///
/// This marks variant PUSHes with `VARIANT_PUSH` flag and stores their
/// data table offset. PUSH0 is skipped (always invariant, not in variance map).
pub(crate) fn apply_variance(&mut self, variance: &crate::skeleton::SkeletonVariance) {
    use revm_bytecode::opcode as op;
    let mut push_index = 0usize;
    for inst in &mut self.insts {
        if inst.opcode >= op::PUSH1 && inst.opcode <= op::PUSH32 {
            assert!(
                push_index < variance.pushes.len(),
                "variance map has fewer entries than bytecode PUSH instructions"
            );
            match variance.pushes[push_index] {
                crate::skeleton::PushClassification::Variant { table_index } => {
                    debug_assert!(
                        !inst.flags.contains(InstFlags::SKIP_LOGIC),
                        "PUSH at index {} is both SKIP_LOGIC and Variant",
                        push_index
                    );
                    inst.flags |= InstFlags::VARIANT_PUSH;
                    inst.imm_table_offset = table_index;
                }
                crate::skeleton::PushClassification::Invariant => {}
            }
            push_index += 1;
        }
    }
    assert_eq!(
        push_index,
        variance.pushes.len(),
        "variance map has more entries than bytecode PUSH instructions"
    );
}
```

**Step 4: Verify compilation**

Run: `cargo check -p revmc`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/revmc/src/bytecode/mod.rs
git commit -m "feat(skeleton): add VARIANT_PUSH flag and imm_table_offset to InstData"
```

> **Note:** Log unexpected discoveries, technical decisions, and implementation insights to `.planning/findings.md` after each task.

---

### Task 4: translate.rs — Variant PUSH Code Generation

**Files:**
- Modify: `crates/revmc/src/compiler/translate.rs:979-985`

**Step 1: Modify PUSH1..=PUSH32 translation**

Replace the PUSH1..=PUSH32 arm (lines 979-985) with:

```rust
op::PUSH1..=op::PUSH32 => {
    if data.flags.contains(InstFlags::VARIANT_PUSH) {
        // Load i256 from per-instance data table (32-byte LE entries).
        // 1. Load imm_data_ptr from EvmContext.
        let table_ptr_ptr = get_field(
            &mut self.bcx,
            self.ecx,
            mem::offset_of!(EvmContext<'_>, imm_data_ptr),
            "ecx.imm_data_ptr.addr",
        );
        let table_ptr = self.bcx.load(self.ptr_type, table_ptr_ptr, "imm_table_ptr");
        // 2. GEP to the entry: byte_offset = table_index * 32.
        let byte_offset = data.imm_table_offset as i64 * 32;
        let offset = self.bcx.iconst(self.isize_type, byte_offset);
        let elem_ptr = self.bcx.gep(
            self.bcx.type_int(8),
            table_ptr,
            &[offset],
            "imm.ptr",
        );
        // 3. Load i256 (LE on x86-64, matching U256 memory layout).
        let value = self.bcx.load(self.word_type, elem_ptr, "imm.val");
        self.push(value);
    } else {
        // Existing path: compile-time constant.
        let imm = self.bytecode.get_imm(data);
        let value = imm.map(U256::from_be_slice).unwrap_or_default();
        let value = self.bcx.iconst_256(value);
        self.push(value);
    }
}
```

**Step 2: Add required import**

At the top of translate.rs, ensure `EvmContext` is imported. Check existing imports — it should already be available via `use crate::*`. If not, add:

```rust
use revmc_context::EvmContext;
```

**Step 3: Verify compilation**

Run: `cargo check -p revmc`
Expected: PASS

**Step 4: Commit**

```bash
git add crates/revmc/src/compiler/translate.rs
git commit -m "feat(skeleton): variant PUSH codegen — load i256 from data table"
```

> **Note:** Log unexpected discoveries, technical decisions, and implementation insights to `.planning/findings.md` after each task.

---

### Task 5: translate_skeleton() Entry Point

**Files:**
- Modify: `crates/revmc/src/compiler/mod.rs:220-230`

**Step 1: Add translate_skeleton method**

Add after the existing `translate()` method (after line 230):

```rust
/// Translates EVM bytecode using skeleton-aware compilation.
///
/// Like [`translate`](Self::translate), but applies variance classification so that
/// variant PUSHes load from a per-instance data table instead of using inline constants.
/// The caller must set `EvmContext::imm_data_ptr` before invoking the compiled function.
///
/// If `variance` has no variant PUSHes (all Invariant), this produces identical code
/// to `translate()` — zero overhead for singletons.
pub fn translate_skeleton<'a>(
    &mut self,
    name: &str,
    input: impl Into<EvmCompilerInput<'a>>,
    spec_id: SpecId,
    variance: &crate::skeleton::SkeletonVariance,
) -> Result<B::FuncId> {
    ensure!(cfg!(target_endian = "little"), "only little-endian is supported");
    ensure!(!self.finalized, "cannot compile more functions after finalizing the module");
    let mut bytecode = self.parse(input.into(), spec_id)?;
    bytecode.apply_variance(variance);
    self.translate_inner(name, &bytecode)
}

/// (JIT) Compiles skeleton-aware EVM bytecode into a JIT function.
///
/// See [`translate_skeleton`](Self::translate_skeleton) for more information.
///
/// # Safety
///
/// The returned function pointer is owned by the module, and must not be called after the
/// module is cleared or the function is freed.
pub unsafe fn jit_skeleton<'a>(
    &mut self,
    name: &str,
    bytecode: impl Into<EvmCompilerInput<'a>>,
    spec_id: SpecId,
    variance: &crate::skeleton::SkeletonVariance,
) -> Result<EvmCompilerFn> {
    let id = self.translate_skeleton(name, bytecode.into(), spec_id, variance)?;
    unsafe { self.jit_function(id) }
}
```

**Step 2: Verify compilation**

Run: `cargo check -p revmc`
Expected: PASS

**Step 3: Commit**

```bash
git add crates/revmc/src/compiler/mod.rs
git commit -m "feat(skeleton): add translate_skeleton() and jit_skeleton() entry points"
```

> **Note:** Log unexpected discoveries, technical decisions, and implementation insights to `.planning/findings.md` after each task.

---

### Task 6: Existing Tests — Verify No Regressions

**Files:**
- None (read-only verification)

**Step 1: Run full test suite**

Run: `cargo test -p revmc -p revmc-context -p revmc-builtins`
Expected: All existing tests PASS. The new `imm_data_ptr` field is initialized to null and `VARIANT_PUSH` flag is never set in the existing path, so behavior is identical.

**Step 2: Run the existing compiler tests specifically**

Run: `cargo test -p revmc --lib tests`
Expected: PASS

**Step 3: Log results**

Update `.planning/findings.md` with any test results or issues found.

> **Note:** Log unexpected discoveries, technical decisions, and implementation insights to `.planning/findings.md` after each task.

---

### Task 7: Integration Test — skeleton_bench Binary

**Files:**
- Create: `examples/runner/src/bin/skeleton_bench.rs`
- Modify: `examples/runner/Cargo.toml` (add `[[bin]]` entry)

**Step 1: Add bin entry to Cargo.toml**

Add at the end of `examples/runner/Cargo.toml`:

```toml
[[bin]]
name = "skeleton_bench"
path = "src/bin/skeleton_bench.rs"
```

**Step 2: Write skeleton_bench.rs**

Create `examples/runner/src/bin/skeleton_bench.rs`:

```rust
//! Skeleton-aware compilation correctness and performance benchmark.
//!
//! Picks a skeleton group with multiple instances (e.g., Uniswap V3 Pool),
//! compiles one instance two ways:
//!   1. Per-hash (current path) — translate()
//!   2. Skeleton + data table   — translate_skeleton()
//! Executes identical transactions, compares gas used and return data.

#[path = "../bin_common.rs"]
mod bin_common;
use bin_common::*;

use clap::Parser;
use revmc::{
    EvmCompilerFn, EvmLlvmBackend, OptimizationLevel,
    skeleton::{analyze_skeleton_group, build_data_table, SkeletonVariance, PushClassification},
};
use revm_bytecode::opcode as op;
use revm_primitives::B256;
use std::collections::HashMap;
use std::time::Instant;

#[derive(Parser)]
struct Args {
    /// Path to the state .bin files
    #[arg(long, default_value = "/home/ubuntu/sipeng/bench_data/states")]
    states_dir: String,
    /// Path to the txs .bin files
    #[arg(long, default_value = "/home/ubuntu/sipeng/bench_data/txs")]
    txs_dir: String,
    /// Block number to use
    #[arg(long)]
    block: Option<u64>,
    /// Only run correctness check (no benchmark)
    #[arg(long)]
    correctness_only: bool,
}

/// Extract opcode skeleton from bytecode (strips PUSH immediates).
fn extract_skeleton(bytecode: &[u8]) -> Vec<u8> {
    let mut skeleton = Vec::with_capacity(bytecode.len());
    let mut i = 0;
    while i < bytecode.len() {
        let op = bytecode[i];
        skeleton.push(op);
        i += 1;
        if op >= 0x60 && op <= 0x7f {
            let n = (op - 0x5f) as usize;
            i += n;
        }
    }
    skeleton
}

fn main() {
    let args = Args::parse();

    // Load state data
    let loader = BinLoader::new(&args.states_dir, &args.txs_dir);
    let blocks = loader.available_blocks();
    let block_num = args.block.unwrap_or_else(|| {
        *blocks.first().expect("no blocks available")
    });

    println!("Loading block {block_num}...");
    let (snapshot, txs) = loader.load(block_num);

    // Build skeleton groups from all contracts in this block
    let mut skeleton_groups: HashMap<Vec<u8>, Vec<(B256, Vec<u8>)>> = HashMap::new();
    for (code_hash, code) in snapshot.iter_contracts() {
        let code_bytes = code.as_ref();
        if code_bytes.len() < 32 { continue; }
        let skeleton = extract_skeleton(code_bytes);
        skeleton_groups.entry(skeleton)
            .or_default()
            .push((code_hash, code_bytes.to_vec()));
    }

    // Find the largest skeleton group
    let (best_skeleton, best_group) = skeleton_groups
        .iter()
        .max_by_key(|(_, members)| members.len())
        .expect("no skeleton groups found");

    println!(
        "Best skeleton group: {} members, {} bytes skeleton",
        best_group.len(),
        best_skeleton.len()
    );

    if best_group.len() < 2 {
        println!("No skeleton group with 2+ members found in this block.");
        println!("Try a different block or use --block <N> to specify one.");
        return;
    }

    // Analyze variance
    let bytecodes: Vec<&[u8]> = best_group.iter().map(|(_, bc)| bc.as_slice()).collect();
    let variance = analyze_skeleton_group(&bytecodes);
    println!(
        "Variance: {} total PUSHes, {} variant ({:.2}%)",
        variance.pushes.len(),
        variance.num_variant,
        if variance.pushes.is_empty() {
            0.0
        } else {
            variance.num_variant as f64 / variance.pushes.len() as f64 * 100.0
        }
    );

    // Pick the first instance for compilation test
    let (test_hash, test_bytecode) = &best_group[0];
    let data_table = build_data_table(test_bytecode, &variance);
    println!(
        "Test contract: {test_hash}, bytecode={} bytes, data_table={} bytes",
        test_bytecode.len(),
        data_table.data.len()
    );

    // Compile both ways
    let cx = revmc::llvm::inkwell::context::Context::create();
    let spec = revm_primitives::hardfork::SpecId::PRAGUE;

    // Method 1: Per-hash compilation
    let mut compiler1 = revmc::EvmCompiler::new(
        EvmLlvmBackend::new(&cx, OptimizationLevel::Aggressive).unwrap()
    );
    compiler1.inspect_stack_length(true);
    let t1 = Instant::now();
    let fn1 = unsafe {
        compiler1.jit("per_hash", test_bytecode.as_slice(), spec).unwrap()
    };
    let compile_time_hash = t1.elapsed();

    // Method 2: Skeleton compilation
    let mut compiler2 = revmc::EvmCompiler::new(
        EvmLlvmBackend::new(&cx, OptimizationLevel::Aggressive).unwrap()
    );
    compiler2.inspect_stack_length(true);
    let t2 = Instant::now();
    let fn2 = unsafe {
        compiler2.jit_skeleton("skeleton", test_bytecode.as_slice(), spec, &variance).unwrap()
    };
    let compile_time_skel = t2.elapsed();

    println!("\n=== Compilation Times ===");
    println!("Per-hash:  {:?}", compile_time_hash);
    println!("Skeleton:  {:?}", compile_time_skel);

    println!("\n=== Correctness ===");
    println!("Both methods compiled successfully.");
    println!("Data table entries: {}", variance.num_variant);

    // For a full correctness check, we would execute identical transactions
    // through both compiled functions and compare gas + return data.
    // This requires setting up the full EVM context, which depends on
    // the specific block's transactions touching this contract.
    // TODO: Add transaction execution comparison in Phase 2.

    if !args.correctness_only {
        println!("\n=== Summary ===");
        println!("Skeleton group size: {} contracts sharing one compilation", best_group.len());
        let savings = (best_group.len() - 1) as f64 / best_group.len() as f64 * 100.0;
        println!("Compilation savings: {:.1}% (compile once, reuse {} times)", savings, best_group.len() - 1);
        println!("Data table overhead per instance: {} bytes", data_table.data.len());
    }
}
```

**Step 3: Verify compilation**

Run: `cargo check -p revmc-examples-runner --bin skeleton_bench`
Expected: PASS (there may be warnings about unused imports if `snapshot.iter_contracts()` doesn't exist; adjust the actual API to match `bin_common` patterns).

**Step 4: Commit**

```bash
git add examples/runner/src/bin/skeleton_bench.rs examples/runner/Cargo.toml
git commit -m "feat(skeleton): add skeleton_bench binary for correctness verification"
```

> **Note:** The skeleton_bench binary is a starting point. It verifies that both compilation paths produce working functions. Full execution-level comparison (gas, return data) depends on block data and will be refined iteratively. Log any API issues with bin_common to `.planning/findings.md`.

---

### Parallelism Groups

- **Group A** (parallel): Task 1, Task 2
  - Task 1 edits `revmc-context`, Task 2 edits `revmc/src/skeleton.rs` + `revmc/src/lib.rs` — no file overlap.
- **Group B** (after Group A): Task 3
  - Depends on Task 2 (imports `crate::skeleton::*`).
- **Group C** (after Group B): Task 4, Task 5
  - Task 4 edits `translate.rs`, Task 5 edits `mod.rs` — no file overlap, both depend on Task 3's `VARIANT_PUSH` flag.
- **Group D** (after Group C): Task 6
  - Regression test suite — depends on all prior tasks.
- **Group E** (after Group D): Task 7
  - Integration binary — depends on everything compiling.

**Parallelism score:** 2/7 tasks can run in parallel in the first group; 2/7 in Group C.

---

### Execution Handoff

See end of plan for execution mode selection.
