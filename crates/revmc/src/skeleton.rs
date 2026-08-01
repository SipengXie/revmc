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

impl ClassifyMode {
    /// Stable tag used in CLI flags, logs, and cache-file names.
    pub fn as_str(self) -> &'static str {
        match self {
            ClassifyMode::Auto => "auto",
            ClassifyMode::ForceAllVariant => "force-all-variant",
        }
    }
}

impl core::str::FromStr for ClassifyMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "auto" => Ok(ClassifyMode::Auto),
            "force-all-variant" => Ok(ClassifyMode::ForceAllVariant),
            other => {
                Err(format!("unknown classify mode '{other}', expected auto|force-all-variant"))
            }
        }
    }
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
    for &bytecode in &bytecodes[1..] {
        let pushes = extract_push_immediates(bytecode);
        assert_eq!(
            pushes.len(),
            ref_pushes.len(),
            "bytecodes have different number of PUSH instructions"
        );
        if mode == ClassifyMode::Auto {
            for (i, (ref_imm, imm)) in ref_pushes.iter().zip(pushes.iter()).enumerate() {
                if ref_imm != imm {
                    is_variant[i] = true;
                }
            }
        }
    }
    if mode == ClassifyMode::ForceAllVariant {
        let skip_logic = skip_logic_push_mask(reference);
        // Hard assert, not debug_assert: the indexing below would otherwise
        // become an out-of-bounds abort in release with no diagnosis.
        assert_eq!(skip_logic.len(), ref_pushes.len());
        for (i, v) in is_variant.iter_mut().enumerate() {
            *v = !skip_logic[i];
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
    for ((declared_len, imm), classification) in pushes.iter().zip(variance.pushes.iter()) {
        if let PushClassification::Variant { table_index } = classification {
            let offset = *table_index as usize * 32;
            let le_bytes = normalize_push_immediate(imm, *declared_len);
            data[offset..offset + 32].copy_from_slice(&le_bytes);
        }
    }

    ImmDataTable { data }
}

/// Normalize a PUSH immediate to its 32-byte LE runtime value.
///
/// `declared_len` is the PUSH opcode's declared width. When the immediate is
/// truncated by the physical end of code, the EVM reads the missing bytes as
/// zero — i.e. the value is zero-extended on the RIGHT (low side), so the
/// present bytes must be shifted up to their declared positions.
pub(crate) fn normalize_push_immediate(imm: &[u8], declared_len: usize) -> [u8; 32] {
    let missing = declared_len.saturating_sub(imm.len());
    (U256::from_be_slice(imm) << (8 * missing)).to_le_bytes::<32>()
}

/// Mask of PUSH1..PUSH32 instructions whose immediate is baked into a static
/// JUMP/JUMPI (what the bytecode analysis would mark as `SKIP_LOGIC`). The mask
/// is indexed by PUSH ordinal, matching [`extract_push_immediates`] (PUSH0 is
/// skipped). A `true` entry means the PUSH's value is consumed by an adjacent
/// `JUMP` or `JUMPI`, so classifying it as Variant would invalidate the
/// static-jump resolution done by the backend.
///
/// Target validity is deliberately NOT required. An invalid target makes the
/// backend emit an unconditional `InvalidJump` trap instead of folding the
/// branch, which is equally incompatible with a runtime table lookup — and
/// worse, produces no assert, so the mismatch would be silent.
pub fn skip_logic_push_mask(bytecode: &[u8]) -> Vec<bool> {
    // Mark PUSH<N> at ordinal k iff it is immediately followed by JUMP/JUMPI
    // and the immediate fits in a usize (wider immediates cannot be a PC, and
    // the backend's own analysis rejects them the same way).
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
                    is_skip = true;
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

/// `true` if any Variant-classified PUSH feeds an adjacent static JUMP/JUMPI
/// (`skip_logic_push_mask`): the backend folds such jumps to compile-time
/// targets, which is incompatible with a runtime table lookup —
/// `apply_variance` asserts on the combination. Families where members
/// genuinely jump to different targets must be degraded to per-member
/// compilation by the caller.
pub fn variance_conflicts_static_jumps(reference: &[u8], variance: &SkeletonVariance) -> bool {
    let skip = skip_logic_push_mask(reference);
    // Hard assert: a silent zip truncation here would reopen the
    // apply_variance abort this pre-check exists to prevent.
    assert_eq!(skip.len(), variance.pushes.len());
    variance
        .pushes
        .iter()
        .zip(skip.iter())
        .any(|(p, s)| *s && matches!(p, PushClassification::Variant { .. }))
}

/// `true` if this family cannot share one artifact because a PUSH consumed by
/// an adjacent static JUMP/JUMPI differs between members.
///
/// Answered from the member bytecodes rather than from an existing
/// [`SkeletonVariance`], because [`ClassifyMode::ForceAllVariant`] forces
/// exactly those PUSHes back to Invariant — so
/// [`variance_conflicts_static_jumps`] applied to a ForceAllVariant variance
/// answers `false` for every family, whatever the members contain. Callers
/// must use this one when the run's classify mode is not fixed to `Auto`.
pub fn members_conflict_static_jumps(bytecodes: &[&[u8]]) -> bool {
    if bytecodes.len() < 2 {
        return false;
    }
    let auto = analyze_skeleton_group_with_mode(bytecodes, ClassifyMode::Auto);
    variance_conflicts_static_jumps(bytecodes[0], &auto)
}

/// Extract the opcode skeleton: all opcodes kept, PUSH1..PUSH32 immediate
/// bytes removed.
pub fn extract_opcode_skeleton(bytecode: &[u8]) -> Vec<u8> {
    let mut skeleton = Vec::with_capacity(bytecode.len());
    let mut i = 0;
    while i < bytecode.len() {
        let op_byte = bytecode[i];
        skeleton.push(op_byte);
        i += 1;
        if op_byte >= op::PUSH1 && op_byte <= op::PUSH32 {
            i += (op_byte - op::PUSH0) as usize;
        }
    }
    skeleton
}

/// Extract PUSH1..PUSH32 immediates as `(declared_len, bytes)` pairs, in
/// opcode order. `bytes` is shorter than `declared_len` only for a PUSH
/// truncated by the physical end of code. Skips PUSH0 (no immediate bytes,
/// always zero).
pub fn extract_push_immediates(bytecode: &[u8]) -> Vec<(usize, &[u8])> {
    let mut result = Vec::new();
    let mut i = 0;
    while i < bytecode.len() {
        let op_byte = bytecode[i];
        i += 1;
        if op_byte >= op::PUSH1 && op_byte <= op::PUSH32 {
            let n = (op_byte - op::PUSH0) as usize;
            let end = (i + n).min(bytecode.len());
            result.push((n, &bytecode[i..end]));
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
        let a = normalize_push_immediate(pushes[0].1, pushes[0].0);
        let b = normalize_push_immediate(pushes[1].1, pushes[1].0);

        assert_eq!(a[0], 0x2a);
        assert_eq!(a[1..], [0u8; 31]);
        assert_eq!(b, [0xff; 32]);
    }

    #[test]
    fn test_truncated_push_immediate_zero_extends_right() {
        // PUSH2 with only one immediate byte present: the EVM reads the
        // missing byte as zero, so the value is 0xAB00, not 0x00AB.
        let bc = &[0x61, 0xAB];
        let pushes = extract_push_immediates(bc);
        assert_eq!(pushes, vec![(2usize, &bc[1..2])]);
        let le = normalize_push_immediate(pushes[0].1, pushes[0].0);
        assert_eq!(le[0], 0x00);
        assert_eq!(le[1], 0xAB);
        assert_eq!(le[2..], [0u8; 30]);

        // End-to-end through the data table with two differing members.
        let bc1 = &[0x61, 0xAB];
        let bc2 = &[0x61, 0xCD];
        let variance = analyze_skeleton_group(&[bc1, bc2]);
        assert_eq!(variance.num_variant, 1);
        let table = build_data_table(bc1, &variance);
        assert_eq!(table.data[0], 0x00);
        assert_eq!(table.data[1], 0xAB);
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
    fn test_variance_conflicts_static_jumps_detection() {
        // Members whose static-jump PUSH targets differ: PUSH1 <t>, JUMP,
        // JUMPDEST@3, JUMPDEST@4, STOP. Both 3 and 4 are valid JUMPDESTs, so
        // both members fold to static jumps, but Auto classifies the PUSH as
        // Variant — exactly the combination apply_variance asserts on.
        let a: &[u8] = &[0x60, 0x03, 0x56, 0x5b, 0x5b, 0x00];
        let b: &[u8] = &[0x60, 0x04, 0x56, 0x5b, 0x5b, 0x00];
        let variance = analyze_skeleton_group_with_mode(&[a, b], ClassifyMode::Auto);
        assert_eq!(variance.num_variant, 1);
        assert!(variance_conflicts_static_jumps(a, &variance));

        // Same shape but identical targets: PUSH stays Invariant, no conflict.
        let v_same = analyze_skeleton_group_with_mode(&[a, a], ClassifyMode::Auto);
        assert!(!variance_conflicts_static_jumps(a, &v_same));

        // Variant PUSH that does NOT feed a jump: no conflict.
        let c: &[u8] = &[0x60, 0x41, 0x00];
        let d: &[u8] = &[0x60, 0x42, 0x00];
        let v_plain = analyze_skeleton_group_with_mode(&[c, d], ClassifyMode::Auto);
        assert_eq!(v_plain.num_variant, 1);
        assert!(!variance_conflicts_static_jumps(c, &v_plain));
    }

    #[test]
    fn conflict_detected_when_reference_target_is_not_a_jumpdest() {
        // PUSH1 <t>, JUMP, JUMPDEST@3, PUSH1 0, PUSH1 0, RETURN.
        // The reference's target (2) is the JUMP byte itself, so the backend
        // bakes an unconditional InvalidJump; member `b` needs a real jump to
        // JUMPDEST@3. Requiring a VALID target here would miss the conflict and
        // hand `b` an artifact that always halts.
        let a: &[u8] = &[0x60, 0x02, 0x56, 0x5b, 0x60, 0x00, 0x60, 0x00, 0xf3];
        let b: &[u8] = &[0x60, 0x03, 0x56, 0x5b, 0x60, 0x00, 0x60, 0x00, 0xf3];
        assert!(members_conflict_static_jumps(&[a, b]));
        assert!(members_conflict_static_jumps(&[b, a]), "must not depend on member order");
    }

    #[test]
    fn force_all_variant_does_not_hide_a_static_jump_conflict() {
        // ForceAllVariant pushes static-jump PUSHes back to Invariant, which
        // makes `variance_conflicts_static_jumps` answer false for every
        // family; the member-level check must still see the conflict.
        let a: &[u8] = &[0x60, 0x03, 0x56, 0x5b, 0x5b, 0x00];
        let b: &[u8] = &[0x60, 0x04, 0x56, 0x5b, 0x5b, 0x00];
        let forced = analyze_skeleton_group_with_mode(&[a, b], ClassifyMode::ForceAllVariant);
        assert!(!variance_conflicts_static_jumps(a, &forced));
        assert!(members_conflict_static_jumps(&[a, b]));
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
