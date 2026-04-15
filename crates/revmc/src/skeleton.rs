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
    Variant {
        /// The variant PUSH ordinal (0, 1, 2, ...).
        table_index: u32,
    },
}

/// Variance map for an opcode skeleton.
/// One entry per PUSH1..PUSH32 instruction, in opcode order.
/// PUSH0 is excluded (value is always 0, always invariant).
#[derive(Clone, Debug)]
pub struct SkeletonVariance {
    /// Classification of each PUSH1..PUSH32, in opcode order.
    pub pushes: Vec<PushClassification>,
    /// Total number of variant PUSHes.
    pub num_variant: u32,
}

/// Per-instance data table: contiguous array of 32-byte LE i256 values,
/// one entry per variant PUSH. Layout: `[variant_0: [u8; 32], variant_1: [u8; 32], ...]`.
#[derive(Debug)]
pub struct ImmDataTable {
    /// Raw bytes: length = num_variant_pushes * 32.
    pub data: Vec<u8>,
}

/// Classification mode for [`analyze_skeleton_group_with_mode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClassifyMode {
    /// Default: compare across instances; only differing PUSHes become Variant.
    Auto,
    /// Ablation: mark every non-PUSH0 as Variant regardless of cross-instance equality.
    /// Disables LLVM constant folding over PUSH immediates while keeping skeleton sharing.
    ForceAllVariant,
}

/// Analyze a group of bytecodes sharing the same opcode skeleton.
/// Compares PUSH1..PUSH32 immediates across all instances to classify each as
/// Invariant (same value everywhere) or Variant (differs in at least one instance).
///
/// # Panics
/// - If `bytecodes` is empty.
/// - If bytecodes don't share the same opcode skeleton (different PUSH counts).
pub fn analyze_skeleton_group(bytecodes: &[&[u8]]) -> SkeletonVariance {
    analyze_skeleton_group_with_mode(bytecodes, ClassifyMode::Auto)
}

/// Variant of [`analyze_skeleton_group`] parameterized by [`ClassifyMode`].
///
/// # Panics
/// Same panic conditions as [`analyze_skeleton_group`].
pub fn analyze_skeleton_group_with_mode(
    bytecodes: &[&[u8]],
    mode: ClassifyMode,
) -> SkeletonVariance {
    assert!(!bytecodes.is_empty(), "need at least one bytecode");

    let reference = bytecodes[0];
    let ref_pushes = extract_push_immediates(reference);

    let mut is_variant = vec![false; ref_pushes.len()];
    match mode {
        ClassifyMode::Auto => {
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
        }
        ClassifyMode::ForceAllVariant => {
            for &bytecode in &bytecodes[1..] {
                let pushes = extract_push_immediates(bytecode);
                assert_eq!(
                    pushes.len(),
                    ref_pushes.len(),
                    "bytecodes have different number of PUSH instructions"
                );
            }
            let skip_logic = skip_logic_push_mask(reference);
            debug_assert_eq!(skip_logic.len(), ref_pushes.len());
            for (i, v) in is_variant.iter_mut().enumerate() {
                *v = !skip_logic[i];
            }
        }
    }

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
            let le_bytes = normalize_push_immediate(imm);
            data[offset..offset + 32].copy_from_slice(&le_bytes);
        }
    }

    ImmDataTable { data }
}

pub(crate) fn normalize_push_immediate(imm: &[u8]) -> [u8; 32] {
    U256::from_be_slice(imm).to_le_bytes::<32>()
}

/// Mask of PUSH1..PUSH32 instructions whose immediate is baked into a static
/// JUMP/JUMPI (what the bytecode analysis would mark as `SKIP_LOGIC`). The mask
/// is indexed by PUSH ordinal, matching [`extract_push_immediates`] (PUSH0 is
/// skipped). A `true` entry means the PUSH's value is consumed by an adjacent
/// `JUMP` or `JUMPI` to a valid JUMPDEST, so classifying it as Variant would
/// invalidate the static-jump folding done by the backend.
pub(crate) fn skip_logic_push_mask(bytecode: &[u8]) -> Vec<bool> {
    // Pass 1: collect valid JUMPDEST PCs (JUMPDEST opcodes not inside a PUSH imm).
    let mut jumpdests = std::collections::HashSet::new();
    let mut i = 0usize;
    while i < bytecode.len() {
        let op_byte = bytecode[i];
        if op_byte == op::JUMPDEST {
            jumpdests.insert(i);
        }
        if op_byte >= op::PUSH1 && op_byte <= op::PUSH32 {
            let n = (op_byte - op::PUSH0) as usize;
            i += 1 + n;
        } else {
            i += 1;
        }
    }

    // Pass 2: walk again, mark PUSH<N> at ordinal k as skip-logic iff it is
    // immediately followed by JUMP/JUMPI to a valid JUMPDEST and the immediate
    // fits in a usize.
    let mut mask = Vec::new();
    let mut i = 0usize;
    while i < bytecode.len() {
        let op_byte = bytecode[i];
        if op_byte >= op::PUSH1 && op_byte <= op::PUSH32 {
            let n = (op_byte - op::PUSH0) as usize;
            let imm_start = i + 1;
            let imm_end = (imm_start + n).min(bytecode.len());
            let jump_pc = imm_end;
            let mut is_skip = false;
            if jump_pc < bytecode.len() && n <= std::mem::size_of::<usize>() {
                let jump_op = bytecode[jump_pc];
                if jump_op == op::JUMP || jump_op == op::JUMPI {
                    let imm = &bytecode[imm_start..imm_end];
                    let mut padded = [0u8; std::mem::size_of::<usize>()];
                    padded[std::mem::size_of::<usize>() - imm.len()..].copy_from_slice(imm);
                    let target = usize::from_be_bytes(padded);
                    if jumpdests.contains(&target) {
                        is_skip = true;
                    }
                }
            }
            mask.push(is_skip);
            i = imm_end;
        } else {
            i += 1;
        }
    }
    mask
}

/// Extract PUSH1..PUSH32 immediate byte slices from bytecode, in opcode order.
/// Skips PUSH0 (no immediate bytes, always zero).
pub(crate) fn extract_push_immediates(bytecode: &[u8]) -> Vec<&[u8]> {
    let mut result = Vec::new();
    let mut i = 0;
    while i < bytecode.len() {
        let op_byte = bytecode[i];
        i += 1;
        if op_byte >= op::PUSH1 && op_byte <= op::PUSH32 {
            let n = (op_byte - op::PUSH0) as usize;
            let end = (i + n).min(bytecode.len());
            result.push(&bytecode[i..end]);
            i = end;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_invariant() {
        let bc = &[0x60, 0x42, 0x61, 0x00, 0x01, 0x00];
        let variance = analyze_skeleton_group(&[bc, bc]);
        assert_eq!(variance.pushes.len(), 2);
        assert_eq!(variance.pushes[0], PushClassification::Invariant);
        assert_eq!(variance.pushes[1], PushClassification::Invariant);
        assert_eq!(variance.num_variant, 0);
    }

    #[test]
    fn test_one_variant() {
        let bc1 = &[0x60, 0x42, 0x60, 0x01, 0x00];
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
        assert_eq!(table1.data.len(), 32);
        assert_eq!(table1.data[0], 0x01);
        assert_eq!(table1.data[1..32], [0u8; 31]);

        let table2 = build_data_table(bc2, &variance);
        assert_eq!(table2.data[0], 0x02);
    }

    #[test]
    fn test_push32_variant() {
        let mut bc1 = vec![0x7f];
        bc1.extend_from_slice(&[0xff; 32]);
        bc1.push(0x00);

        let mut bc2 = vec![0x7f];
        bc2.extend_from_slice(&[0xaa; 32]);
        bc2.push(0x00);

        let variance = analyze_skeleton_group(&[&bc1, &bc2]);
        assert_eq!(variance.num_variant, 1);

        let table = build_data_table(&bc1, &variance);
        assert!(table.data.iter().all(|&b| b == 0xff));
    }

    #[test]
    fn test_normalize_push_immediate_matches_build_data_table_semantics() {
        let mut bc = vec![0x60, 0x2a, 0x7f];
        bc.extend_from_slice(&[0xff; 32]);

        let pushes = extract_push_immediates(&bc);
        let a = normalize_push_immediate(pushes[0]);
        let b = normalize_push_immediate(pushes[1]);

        assert_eq!(a[0], 0x2a);
        assert_eq!(a[1..], [0u8; 31]);
        assert_eq!(b, [0xff; 32]);
    }

    #[test]
    fn test_singleton_all_invariant() {
        let bc = &[0x60, 0x42, 0x60, 0x01, 0x00];
        let variance = analyze_skeleton_group(&[bc]);
        assert_eq!(variance.num_variant, 0);
        assert!(variance.pushes.iter().all(|p| *p == PushClassification::Invariant));
    }

    #[test]
    fn test_force_all_variant_marks_every_non_push0_as_variant() {
        let bc = &[0x5f, 0x60, 0x42, 0x61, 0x00, 0x01, 0x00];

        let auto = analyze_skeleton_group_with_mode(&[bc, bc], ClassifyMode::Auto);
        assert_eq!(auto.num_variant, 0);

        let forced = analyze_skeleton_group_with_mode(&[bc, bc], ClassifyMode::ForceAllVariant);
        assert_eq!(forced.pushes.len(), 2);
        assert_eq!(forced.pushes[0], PushClassification::Variant { table_index: 0 });
        assert_eq!(forced.pushes[1], PushClassification::Variant { table_index: 1 });
        assert_eq!(forced.num_variant, 2);
    }

    #[test]
    fn test_force_all_variant_data_table_roundtrips_values() {
        let bc1 = &[0x60, 0x42, 0x60, 0x01, 0x00];
        let bc2 = &[0x60, 0x42, 0x60, 0x02, 0x00];
        let variance = analyze_skeleton_group_with_mode(&[bc1, bc2], ClassifyMode::ForceAllVariant);
        assert_eq!(variance.num_variant, 2);

        let table = build_data_table(bc1, &variance);
        assert_eq!(table.data.len(), 64);
        assert_eq!(table.data[0], 0x42);
        assert_eq!(table.data[32], 0x01);
    }

    #[test]
    fn test_force_all_variant_preserves_skip_logic() {
        // PUSH1 0x03, JUMP, JUMPDEST, STOP — JUMPDEST sits at PC 3, PUSH1 pushes
        // 3, so the PUSH immediate is a valid static-jump target. Bytecode
        // analysis would flag it SKIP_LOGIC, and ForceAllVariant must leave it
        // Invariant to preserve static-jump folding.
        let bc = &[0x60, 0x03, 0x56, 0x5b, 0x00];
        let variance = analyze_skeleton_group_with_mode(&[bc, bc], ClassifyMode::ForceAllVariant);
        assert_eq!(variance.pushes.len(), 1);
        assert_eq!(variance.pushes[0], PushClassification::Invariant);
        assert_eq!(variance.num_variant, 0);

        // Add a second, non-SKIP_LOGIC PUSH1 — it must still be forced Variant.
        // Layout: PUSH1 0x03, JUMP, JUMPDEST, PUSH1 0x42, STOP
        let bc2 = &[0x60, 0x03, 0x56, 0x5b, 0x60, 0x42, 0x00];
        let v2 = analyze_skeleton_group_with_mode(&[bc2, bc2], ClassifyMode::ForceAllVariant);
        assert_eq!(v2.pushes.len(), 2);
        assert_eq!(v2.pushes[0], PushClassification::Invariant);
        assert_eq!(v2.pushes[1], PushClassification::Variant { table_index: 0 });
        assert_eq!(v2.num_variant, 1);
    }

    #[test]
    fn test_auto_wrapper_matches_prior_behavior() {
        let bc1 = &[0x60, 0x42, 0x60, 0x01, 0x00];
        let bc2 = &[0x60, 0x42, 0x60, 0x02, 0x00];
        let via_wrapper = analyze_skeleton_group(&[bc1, bc2]);
        let via_mode = analyze_skeleton_group_with_mode(&[bc1, bc2], ClassifyMode::Auto);
        assert_eq!(via_wrapper.num_variant, via_mode.num_variant);
        assert_eq!(via_wrapper.pushes, via_mode.pushes);
    }
}
