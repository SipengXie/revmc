//! PUSH-extraction helpers and unit tests for `push_variance`. Split out of
//! `push_variance.rs`; the parent binary declares `mod skelot_common`,
//! referenced here via `crate::`.

use revmc::emse::SkeletonMode;

use crate::skelot_common::{extract_emse_skeleton, hash_bytes, strip_emse_suffixes};

/// A single PUSH instruction's position and value in a bytecode.
#[derive(Clone)]
pub struct PushInfo {
    /// Index of this PUSH in the opcode stream (0-based, counting only opcodes not imm bytes)
    pub opcode_index: usize,
    /// The PUSH opcode (0x60..0x7f)
    pub opcode: u8,
    /// The immediate value bytes (big-endian)
    pub value: Vec<u8>,
}

/// Extract all PUSH instructions with their positions and values.
pub fn extract_pushes(bytes: &[u8]) -> Vec<PushInfo> {
    let mut pushes = Vec::new();
    let mut i = 0;
    let mut opcode_index = 0;
    while i < bytes.len() {
        let op = bytes[i];
        i += 1;
        if op >= 0x60 && op <= 0x7f {
            let n = (op - 0x5f) as usize;
            let value = if i + n <= bytes.len() {
                bytes[i..i + n].to_vec()
            } else {
                // Truncated PUSH at end of bytecode
                let available = bytes.len().saturating_sub(i);
                bytes[i..i + available].to_vec()
            };
            pushes.push(PushInfo { opcode_index, opcode: op, value });
            i += n;
        }
        opcode_index += 1;
    }
    pushes
}

pub fn hash_skeleton(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
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
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    skeleton.hash(&mut hasher);
    hasher.finish()
}

pub fn hash_skeleton_for_mode(bytes: &[u8], mode: SkeletonMode) -> u64 {
    match mode {
        SkeletonMode::Current => hash_skeleton(bytes),
        SkeletonMode::Emse => hash_bytes(&extract_emse_skeleton(bytes)),
    }
}

pub fn bytecode_view_for_mode(bytes: &[u8], mode: SkeletonMode) -> &[u8] {
    match mode {
        SkeletonMode::Current => bytes,
        SkeletonMode::Emse => strip_emse_suffixes(bytes),
    }
}

pub fn extract_pushes_for_mode(bytes: &[u8], mode: SkeletonMode) -> Vec<PushInfo> {
    extract_pushes(bytecode_view_for_mode(bytes, mode))
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::skelot_common::{extract_emse_skeleton, strip_emse_suffixes};

    fn with_metadata(mut code: Vec<u8>, metadata: &[u8]) -> Vec<u8> {
        code.extend_from_slice(metadata);
        code.extend_from_slice(&(metadata.len() as u16).to_be_bytes());
        code
    }

    #[test]
    fn current_mode_matches_legacy_skeleton_grouping() {
        let a = vec![0x60, 0x11, 0x01];
        let b = vec![0x60, 0x22, 0x01];
        let with_suffix =
            with_metadata(a.clone(), &[0xa1, 0x64, b's', b'o', b'l', b'c', 0x43, 0x00, 0x08, 0x11]);

        assert_eq!(hash_skeleton_for_mode(&a, SkeletonMode::Current), hash_skeleton(&a));
        assert_eq!(hash_skeleton_for_mode(&b, SkeletonMode::Current), hash_skeleton(&b));
        assert_eq!(
            hash_skeleton_for_mode(&a, SkeletonMode::Current),
            hash_skeleton_for_mode(&b, SkeletonMode::Current)
        );
        assert_ne!(
            hash_skeleton_for_mode(&a, SkeletonMode::Current),
            hash_skeleton_for_mode(&with_suffix, SkeletonMode::Current)
        );
    }

    #[test]
    fn emse_groups_bytecodes_differing_only_in_metadata_or_trailing_zeroes() {
        let with_metadata = with_metadata(
            vec![0x60, 0x11, 0x01],
            &[0xa1, 0x64, b's', b'o', b'l', b'c', 0x43, 0x00, 0x08, 0x11],
        );
        let with_trailing_zeroes = vec![0x60, 0x11, 0x01, 0x00, 0x00];

        assert_ne!(
            hash_skeleton_for_mode(&with_metadata, SkeletonMode::Current),
            hash_skeleton_for_mode(&with_trailing_zeroes, SkeletonMode::Current)
        );
        assert_eq!(
            hash_skeleton_for_mode(&with_metadata, SkeletonMode::Emse),
            hash_skeleton_for_mode(&with_trailing_zeroes, SkeletonMode::Emse)
        );
    }

    #[test]
    fn emse_extracts_push_positions_from_stripped_code_view() {
        let bytecode = with_metadata(
            vec![0x60, 0xaa, 0x61, 0xbb, 0xcc, 0x01, 0x00, 0x00],
            &[0xa1, 0x64, b's', b'o', b'l', b'c', 0x43, 0x00, 0x08, 0x11],
        );

        assert_eq!(
            bytecode_view_for_mode(&bytecode, SkeletonMode::Emse),
            &[0x60, 0xaa, 0x61, 0xbb, 0xcc, 0x01]
        );

        let pushes = extract_pushes_for_mode(&bytecode, SkeletonMode::Emse);
        assert_eq!(pushes.len(), 2);
        assert_eq!(pushes[0].opcode_index, 0);
        assert_eq!(pushes[0].opcode, 0x60);
        assert_eq!(pushes[0].value, vec![0xaa]);
        assert_eq!(pushes[1].opcode_index, 1);
        assert_eq!(pushes[1].opcode, 0x61);
        assert_eq!(pushes[1].value, vec![0xbb, 0xcc]);
    }
}
