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
#[derive(Debug)]
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

/// Analyze a group of bytecodes sharing the same opcode skeleton.
/// Compares PUSH1..PUSH32 immediates across all instances to classify each as
/// Invariant (same value everywhere) or Variant (differs in at least one instance).
///
/// # Panics
/// - If `bytecodes` is empty.
/// - If bytecodes don't share the same opcode skeleton (different PUSH counts).
pub fn analyze_skeleton_group(bytecodes: &[&[u8]]) -> SkeletonVariance {
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
        for (i, (ref_imm, imm)) in ref_pushes.iter().zip(pushes.iter()).enumerate() {
            if ref_imm != imm {
                is_variant[i] = true;
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
    fn test_singleton_all_invariant() {
        let bc = &[0x60, 0x42, 0x60, 0x01, 0x00];
        let variance = analyze_skeleton_group(&[bc]);
        assert_eq!(variance.num_variant, 0);
        assert!(variance.pushes.iter().all(|p| *p == PushClassification::Invariant));
    }
}
