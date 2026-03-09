# Skeleton-Aware Compilation Design

**Date**: 2026-03-09
**Status**: Draft
**Branch**: jit-integration

## Problem

The current JIT/AOT compiler treats every unique bytecode (by code_hash) as an independent
compilation unit. But many deployed contracts share identical opcode structure — they differ
only in PUSH immediate values (embedded addresses, immutable variables, deployment config).

Measured across ~10K mainnet blocks:
- 19,197 unique bytecodes → 11,259 unique opcode skeletons (41.4% redundant compilations)
- Top skeleton: Uniswap V3 Pool, 3,323 copies differing only in token addresses/fee
- 99.37% of PUSH positions are invariant across skeleton instances; only 0.63% vary

This redundancy directly causes:
1. **L1i cache thrashing**: 3,323 identical-structure functions compete for 128KB L1i
2. **Wasted compilation time**: each copy is independently compiled through LLVM O3
3. **Wasted AOT cache storage**: each copy gets its own .so file

## Solution: Selective Parameterization

Compile each skeleton once. For the 0.63% of PUSH positions that vary across instances,
load values from a per-instance **data table** at runtime instead of embedding as LLVM
constants. The 99.37% invariant PUSHes remain as `iconst_256()` — full LLVM constant
folding preserved.

## Scope (Phase 1: Compiler Core)

This design covers only the compiler core changes. AOT cache integration and JIT runtime
lookup changes are deferred to Phase 2.

## Architecture

### Data Flow

```
                  External pre-analysis
                  ┌─────────────────────────────┐
                  │ 1. Group bytecodes by skel   │
                  │ 2. Compare PUSH values       │
                  │ 3. Classify invariant/variant │
                  │ 4. Build SkeletonVariance     │
                  │ 5. Build ImmDataTable/inst    │
                  └──────────┬──────────────────┘
                             │
                             ▼
                  EvmCompiler::translate_skeleton()
                  ┌─────────────────────────────┐
                  │ Bytecode::new() + analyze()  │
                  │ Apply variance → InstFlags   │
                  │ FunctionCx::translate()      │
                  │   invariant PUSH → iconst256 │
                  │   variant PUSH   → load tbl  │
                  │ finalize() + optimize()      │
                  └──────────┬──────────────────┘
                             │
                             ▼
                  Compiled function (shared)
                  + per-instance ImmDataTable
```

### Layer 1: EvmContext Extension

**File**: `crates/revmc-context/src/lib.rs`

Add a pointer to the per-instance immediate data table:

```rust
#[repr(C)]
pub struct EvmContext<'a> {
    // ... existing fields (0..88) ...
    pub bytecode_ptr: *const u8,    // offset 80
    pub bytecode_len: usize,        // offset 88
    pub imm_data_ptr: *const u8,    // offset 96  ← NEW
}
// Size: 96 → 104 bytes
```

- Update static size/offset assertions
- Initialize to `std::ptr::null()` in `from_interpreter_with_stack()`
- Caller sets `ecx.imm_data_ptr` before invoking compiled function

### Layer 2: Data Structures

**New file**: `crates/revmc/src/skeleton.rs`

```rust
/// Classification of each PUSH in an opcode skeleton.
pub enum PushClassification {
    /// Same value across all instances → compile as iconst_256
    Invariant,
    /// Different values across instances → load from data table at offset
    Variant { table_offset: u32 },
}

/// Variance map for a skeleton, one entry per PUSH instruction in opcode order.
pub struct SkeletonVariance {
    pub pushes: Vec<PushClassification>,
}

/// Per-instance data table: array of 32-byte little-endian i256 values,
/// one entry per variant PUSH. Offset = variant_index * 32.
pub struct ImmDataTable {
    pub data: Vec<u8>,  // len = num_variant_pushes * 32
}

/// Build variance map by comparing PUSH values across bytecodes of same skeleton.
/// PUSH0 is always skipped (value is always 0).
pub fn analyze_skeleton_group(bytecodes: &[&[u8]]) -> SkeletonVariance { ... }

/// Build data table for a specific bytecode instance given a variance map.
/// Each variant PUSH value is stored as 32-byte little-endian i256.
pub fn build_data_table(bytecode: &[u8], variance: &SkeletonVariance) -> ImmDataTable { ... }
```

**Data table format**: Each variant PUSH value is stored as a 32-byte little-endian `i256`,
regardless of original PUSH width (PUSH1..PUSH32). This avoids bswap/zext complexity —
the generated code is a single `load i256` per variant PUSH. Cost: ~20% larger data tables
(e.g., UniV3 Pool: 1056B vs 882B packed), but tables are tiny and fit in L1d.

### Layer 3: InstData Extension

**File**: `crates/revmc/src/bytecode/mod.rs`

```rust
pub(crate) struct InstData {
    pub(crate) opcode: u8,
    pub(crate) flags: InstFlags,
    pub(crate) base_gas: u16,
    pub(crate) data: u32,
    pub(crate) pc: u32,
    pub(crate) section: Section,
    pub(crate) imm_table_offset: u32,  // ← NEW (only meaningful when VARIANT_PUSH set)
}

bitflags! {
    pub(crate) struct InstFlags: u8 {
        // ... existing flags ...
        const VARIANT_PUSH = 1 << N;  // ← NEW
    }
}
```

After `Bytecode::analyze()`, apply variance info:

```rust
/// Apply skeleton variance classification to instruction flags.
pub(crate) fn apply_variance(&mut self, variance: &SkeletonVariance) {
    let mut push_index = 0;
    for inst in &mut self.insts {
        if inst.opcode >= op::PUSH1 && inst.opcode <= op::PUSH32 {
            match variance.pushes[push_index] {
                PushClassification::Variant { table_offset } => {
                    // SKIP_LOGIC PUSHes (static jump targets) must be invariant
                    debug_assert!(
                        !inst.flags.contains(InstFlags::SKIP_LOGIC),
                        "PUSH at opcode index {} is both SKIP_LOGIC and Variant",
                        push_index
                    );
                    inst.flags |= InstFlags::VARIANT_PUSH;
                    inst.imm_table_offset = table_offset;
                }
                PushClassification::Invariant => {}
            }
            push_index += 1;
        }
    }
}
```

### Layer 4: PUSH Translation

**File**: `crates/revmc/src/compiler/translate.rs`

```rust
op::PUSH1..=op::PUSH32 => {
    if data.flags.contains(InstFlags::VARIANT_PUSH) {
        // Load i256 from per-instance data table (32-byte LE entries)
        let table_ptr_ptr = self.get_field(
            self.ecx,
            mem::offset_of!(EvmContext<'_>, imm_data_ptr),
            "ecx.imm_data_ptr.addr",
        );
        let table_ptr = self.bcx.load(self.ptr_type, table_ptr_ptr, "imm_table_ptr");
        // Each entry is 32 bytes, offset = imm_table_offset * 32
        let byte_offset = data.imm_table_offset as i64 * 32;
        let offset = self.bcx.iconst(self.isize_type, byte_offset);
        let elem_ptr = self.bcx.gep(self.bcx.type_int(8), table_ptr, &[offset], "imm.ptr");
        // Uniform load: always i256, stored little-endian (native on x86)
        let value = self.bcx.load(self.word_type, elem_ptr, "imm.val");
        self.push(value);
    } else {
        // Existing path: compile-time constant (unchanged)
        let imm = self.bytecode.get_imm(data);
        let value = imm.map(U256::from_be_slice).unwrap_or_default();
        let value = self.bcx.iconst_256(value);
        self.push(value);
    }
}
```

Note: `imm_table_offset` is the variant PUSH **index** (0, 1, 2, ...), not byte offset.
The byte offset is computed as `index * 32` since every entry is a 32-byte LE i256.

### Compiler Entry Point

**File**: `crates/revmc/src/compiler/mod.rs`

New method alongside existing `translate()`:

```rust
pub fn translate_skeleton<'a>(
    &mut self,
    name: &str,
    bytecode: &'a [u8],
    spec_id: SpecId,
    variance: &SkeletonVariance,
) -> Result<B::FuncId> {
    // Same as translate(), but after analyze(), call apply_variance()
}
```

Existing `translate()` remains untouched — zero impact on current users.

## Singleton Handling

For bytecodes with no skeleton siblings (10,831 singletons), the external pre-analysis
produces a SkeletonVariance where ALL pushes are `Invariant`. The compiler generates
identical code to the current per-hash path — zero overhead, zero behavioral change.

## Verification Strategy

Single-contract comparison benchmark:
1. Pick a skeleton group with multiple instances (e.g., Uniswap V3 Pool)
2. Compile one instance with current per-hash path
3. Compile skeleton with selective parameterization + data table for same instance
4. Execute identical transactions, compare: gas used, return data, state changes
5. Benchmark performance difference

## Key Invariants

- **CODESIZE**: skeleton-invariant (same PUSH widths → same bytecode length)
- **PC**: skeleton-invariant (same opcode positions)
- **Static jumps**: skeleton-invariant (resolved at analysis time, targets same JUMPDESTs)
- **SKIP_LOGIC PUSHes**: unaffected (no IR generated regardless)
- **Backwards compatibility**: existing `translate()` API unchanged

## Risks and Mitigations

| Risk | Mitigation |
|------|-----------|
| LLVM may not optimize loads from data table as well as constants | Only 0.63% of PUSHes affected; these are data values, not control flow |
| bswap + zext overhead for variant PUSHes | ~5 extra instructions per variant PUSH, negligible vs EVM execution cost |
| InstData size increase | u32 field adds 4 bytes per instruction; acceptable for compilation phase |
| Incorrect variance classification | Unit test: compile + execute with data table, compare against per-hash |

## Files Changed (Summary)

| File | Change |
|------|--------|
| `crates/revmc-context/src/lib.rs` | Add `imm_data_ptr` field to EvmContext |
| `crates/revmc/src/skeleton.rs` | NEW: SkeletonVariance, ImmDataTable, analyze/build functions |
| `crates/revmc/src/bytecode/mod.rs` | Add `imm_table_offset` to InstData, `VARIANT_PUSH` flag |
| `crates/revmc/src/compiler/translate.rs` | Variant PUSH path: load from data table |
| `crates/revmc/src/compiler/mod.rs` | New `translate_skeleton()` method |
| `crates/revmc/src/lib.rs` | Re-export skeleton module |
| `examples/runner/src/bin/skeleton_bench.rs` | NEW: correctness + performance verification |
