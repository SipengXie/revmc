use crate::skeleton::{
    ImmDataTable, PushClassification, SkeletonVariance, extract_push_immediates, normalize_push_immediate,
};

const ADMISSION_DESCRIPTOR_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AdmissionDescriptor {
    descriptor_version: u32,
    skeleton_fingerprint: u64,
    variant_positions: Vec<u32>,
    invariant_values: Vec<(u32, [u8; 32])>,
}

impl AdmissionDescriptor {
    pub(crate) fn from_bytecode(
        skeleton_fingerprint: u64,
        bytecode: &[u8],
        variance: &SkeletonVariance,
    ) -> Self {
        let descriptor_version = ADMISSION_DESCRIPTOR_VERSION;
        let pushes = extract_push_immediates(bytecode);
        assert_eq!(
            pushes.len(),
            variance.pushes.len(),
            "variance map doesn't match bytecode PUSH count"
        );

        let mut variant_positions = Vec::new();
        let mut invariant_values = Vec::new();

        for (ordinal, ((declared_len, imm), classification)) in
            pushes.iter().zip(variance.pushes.iter()).enumerate()
        {
            match classification {
                PushClassification::Variant { .. } => variant_positions.push(ordinal as u32),
                PushClassification::Invariant => {
                    invariant_values
                        .push((ordinal as u32, normalize_push_immediate(imm, *declared_len)));
                }
            }
        }

        Self {
            descriptor_version,
            skeleton_fingerprint,
            variant_positions,
            invariant_values,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), DescriptorError> {
        if self.descriptor_version != ADMISSION_DESCRIPTOR_VERSION {
            return Err(DescriptorError::UnsupportedVersion(self.descriptor_version));
        }

        if self.variant_positions.windows(2).any(|w| w[0] >= w[1]) {
            return Err(DescriptorError::NonMonotonicOrdinals);
        }
        if self.invariant_values.windows(2).any(|w| w[0].0 >= w[1].0) {
            return Err(DescriptorError::NonMonotonicOrdinals);
        }

        let mut vi = 0usize;
        let mut ii = 0usize;
        while vi < self.variant_positions.len() && ii < self.invariant_values.len() {
            let v = self.variant_positions[vi];
            let i = self.invariant_values[ii].0;
            if v == i {
                return Err(DescriptorError::OverlappingOrdinals { push_ordinal: v });
            }
            if v < i {
                vi += 1;
            } else {
                ii += 1;
            }
        }

        Ok(())
    }
}

pub(crate) fn check_admission(
    skeleton_fingerprint: u64,
    candidate: &[u8],
    descriptor: &AdmissionDescriptor,
) -> Result<ImmDataTable, AdmissionError> {
    descriptor.validate().map_err(AdmissionError::Descriptor)?;

    if skeleton_fingerprint != descriptor.skeleton_fingerprint {
        return Err(AdmissionError::Mismatch(
            AdmissionMismatch::SkeletonFingerprintMismatch,
        ));
    }

    let pushes = extract_push_immediates(candidate);
    let get_push = |ordinal: u32| -> Result<(usize, &[u8]), AdmissionError> {
        pushes.get(ordinal as usize).copied().ok_or(AdmissionError::Descriptor(
            DescriptorError::OrdinalOutOfRange { push_ordinal: ordinal },
        ))
    };

    for &(ordinal, expected) in &descriptor.invariant_values {
        let (declared_len, imm) = get_push(ordinal)?;
        let actual = normalize_push_immediate(imm, declared_len);
        if actual != expected {
            return Err(AdmissionError::Mismatch(AdmissionMismatch::InvariantMismatch {
                push_ordinal: ordinal,
                expected,
                actual,
            }));
        }
    }

    let mut data = vec![0u8; descriptor.variant_positions.len() * 32];
    for (table_index, &ordinal) in descriptor.variant_positions.iter().enumerate() {
        let (declared_len, imm) = get_push(ordinal)?;
        let value = normalize_push_immediate(imm, declared_len);
        let offset = table_index * 32;
        data[offset..offset + 32].copy_from_slice(&value);
    }

    Ok(ImmDataTable { data })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AdmissionMismatch {
    SkeletonFingerprintMismatch,
    InvariantMismatch {
        push_ordinal: u32,
        expected: [u8; 32],
        actual: [u8; 32],
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DescriptorError {
    UnsupportedVersion(u32),
    OverlappingOrdinals { push_ordinal: u32 },
    NonMonotonicOrdinals,
    OrdinalOutOfRange { push_ordinal: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AdmissionError {
    Descriptor(DescriptorError),
    Mismatch(AdmissionMismatch),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skeleton::{analyze_skeleton_group, build_data_table};

    #[test]
    fn test_build_descriptor_records_variant_positions_and_invariants() {
        let a = &[0x60, 0x11, 0x60, 0x01, 0x60, 0xaa, 0x00];
        let b = &[0x60, 0x11, 0x60, 0x02, 0x60, 0xaa, 0x00];
        let variance = analyze_skeleton_group(&[a, b]);

        let descriptor = AdmissionDescriptor::from_bytecode(0xdead_beef, a, &variance);

        assert_eq!(descriptor.descriptor_version, 1);
        assert_eq!(descriptor.skeleton_fingerprint, 0xdead_beef);
        assert_eq!(descriptor.variant_positions, vec![1]);
        assert_eq!(descriptor.invariant_values.len(), 2);
        assert_eq!(
            descriptor.invariant_values[0],
            (0, [
                0x11, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            ])
        );
        assert_ne!(
            descriptor.invariant_values[0].1,
            [
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x11,
            ]
        );
        assert_eq!(
            descriptor.invariant_values[1],
            (2, [
                0xaa, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            ])
        );
    }

    #[test]
    fn test_check_admission_accepts_matching_candidate_and_builds_table() {
        let a = &[0x60, 0x11, 0x60, 0x01, 0x60, 0xaa, 0x00];
        let b = &[0x60, 0x11, 0x60, 0x02, 0x60, 0xaa, 0x00];
        let variance = analyze_skeleton_group(&[a, b]);
        let descriptor = AdmissionDescriptor::from_bytecode(0xbeef, a, &variance);

        let table = check_admission(0xbeef, b, &descriptor).unwrap();
        let expected = build_data_table(b, &variance);

        assert_eq!(table.data, expected.data);
    }

    #[test]
    fn test_check_admission_returns_empty_table_for_all_invariant_family() {
        let a = &[0x60, 0x11, 0x60, 0xaa, 0x00];
        let variance = analyze_skeleton_group(&[a]);
        let descriptor = AdmissionDescriptor::from_bytecode(0x99, a, &variance);

        let table = check_admission(0x99, a, &descriptor).unwrap();
        assert!(table.data.is_empty());
    }

    #[test]
    fn test_check_admission_rejects_invariant_mismatch() {
        let a = &[0x60, 0x11, 0x60, 0x01, 0x60, 0xaa, 0x00];
        let b = &[0x60, 0x22, 0x60, 0x02, 0x60, 0xaa, 0x00];
        let variance =
            analyze_skeleton_group(&[a, &[0x60, 0x11, 0x60, 0x02, 0x60, 0xaa, 0x00]]);
        let descriptor = AdmissionDescriptor::from_bytecode(0xbeef, a, &variance);

        assert!(matches!(
            check_admission(0xbeef, b, &descriptor),
            Err(AdmissionError::Mismatch(AdmissionMismatch::InvariantMismatch {
                push_ordinal: 0,
                ..
            }))
        ));
    }

    #[test]
    fn test_check_admission_rejects_skeleton_fingerprint_mismatch() {
        let a = &[0x60, 0x11, 0x60, 0x01, 0x00];
        let variance = analyze_skeleton_group(&[a]);
        let descriptor = AdmissionDescriptor::from_bytecode(7, a, &variance);

        assert!(matches!(
            check_admission(8, a, &descriptor),
            Err(AdmissionError::Mismatch(
                AdmissionMismatch::SkeletonFingerprintMismatch
            ))
        ));
    }

    #[test]
    fn test_check_admission_accepts_all_variant_family() {
        let a = &[0x60, 0x01, 0x60, 0x02, 0x00];
        let b = &[0x60, 0x03, 0x60, 0x04, 0x00];
        let variance = analyze_skeleton_group(&[a, b]);
        let descriptor = AdmissionDescriptor::from_bytecode(5, a, &variance);

        let table = check_admission(5, b, &descriptor).unwrap();
        assert_eq!(table.data.len(), 64);
    }

    #[test]
    fn test_check_admission_matches_truncated_immediate_behavior() {
        let a = &[0x61, 0x12, 0x00];
        let variance = analyze_skeleton_group(&[a]);
        let descriptor = AdmissionDescriptor::from_bytecode(3, a, &variance);

        let table = check_admission(3, a, &descriptor).unwrap();
        assert!(table.data.is_empty());
    }

    #[test]
    fn test_validate_descriptor_rejects_overlapping_ordinals() {
        let descriptor = AdmissionDescriptor {
            descriptor_version: 1,
            skeleton_fingerprint: 7,
            variant_positions: vec![1],
            invariant_values: vec![(1, [0u8; 32])],
        };

        assert_eq!(
            descriptor.validate(),
            Err(DescriptorError::OverlappingOrdinals { push_ordinal: 1 })
        );
    }

    #[test]
    fn test_validate_descriptor_rejects_non_monotonic_variant_positions() {
        let descriptor = AdmissionDescriptor {
            descriptor_version: 1,
            skeleton_fingerprint: 7,
            variant_positions: vec![2, 1],
            invariant_values: vec![],
        };

        assert_eq!(
            descriptor.validate(),
            Err(DescriptorError::NonMonotonicOrdinals)
        );
    }

    #[test]
    fn test_validate_descriptor_rejects_unsupported_version() {
        let descriptor = AdmissionDescriptor {
            descriptor_version: ADMISSION_DESCRIPTOR_VERSION + 1,
            skeleton_fingerprint: 7,
            variant_positions: vec![],
            invariant_values: vec![],
        };

        assert_eq!(
            descriptor.validate(),
            Err(DescriptorError::UnsupportedVersion(ADMISSION_DESCRIPTOR_VERSION + 1))
        );
    }

    /// Real EIP-1167 minimal proxy bytecodes from Base Mainnet block 38004930.
    /// All 10 share skeleton bf4abe9c82c9be73 (45 bytes, 1 variant PUSH20 = the
    /// delegate address, 1 invariant PUSH1 = 0x2b).
    #[test]
    fn test_admission_with_real_eip1167_proxies() {
        let proxies: [&[u8]; 10] = [
            &hex_literal::hex!("363d3d373d3d3d363d7350fc21f951bc42421815e104d21c5f094da9e70c5af43d82803e903d91602b57fd5bf3"),
            &hex_literal::hex!("363d3d373d3d3d363d738f41018a8a6e64043d842ae73fffae015a1113495af43d82803e903d91602b57fd5bf3"),
            &hex_literal::hex!("363d3d373d3d3d363d737cad62748ddf516cf85bc2c05c14786d84cf861c5af43d82803e903d91602b57fd5bf3"),
            &hex_literal::hex!("363d3d373d3d3d363d73ec8e5342b19977b4ef8892e02d8daecfa13158315af43d82803e903d91602b57fd5bf3"),
            &hex_literal::hex!("363d3d373d3d3d363d7388cc4e08c7608723f3e44e17ac669fb43b6a83135af43d82803e903d91602b57fd5bf3"),
            &hex_literal::hex!("363d3d373d3d3d363d73942e97a4c6fdc38b4cd1c0298d37d81fdd8e5a165af43d82803e903d91602b57fd5bf3"),
            &hex_literal::hex!("363d3d373d3d3d363d73c80c882c4d8b46700a04a15e7492f6d075b7e8035af43d82803e903d91602b57fd5bf3"),
            &hex_literal::hex!("363d3d373d3d3d363d73d3c6092eaa200edb6830180f20d1cfaab53fe99a5af43d82803e903d91602b57fd5bf3"),
            &hex_literal::hex!("363d3d373d3d3d363d73e26dc80e2b6a08a5d23487e57c3e13a7dfc85f5af43d82803e903d91602b57fd5bf3"),
            &hex_literal::hex!("363d3d373d3d3d363d7310ee18f2789bbf68a2d2c41df5b7c60b08dedd5af43d82803e903d91602b57fd5bf3"),
        ];

        let refs: Vec<&[u8]> = proxies.iter().map(|p| *p).collect();
        let variance = analyze_skeleton_group(&refs);

        let anchor = proxies[0];
        let descriptor = AdmissionDescriptor::from_bytecode(0xbf4abe9c, anchor, &variance);

        for (i, candidate) in proxies.iter().enumerate() {
            let table = check_admission(0xbf4abe9c, candidate, &descriptor)
                .unwrap_or_else(|e| panic!("member {} rejected: {:?}", i, e));
            let expected = build_data_table(candidate, &variance);
            assert_eq!(
                table.data, expected.data,
                "data table mismatch for member {}",
                i
            );
        }
    }
}
