//! EMSE-style skeleton boundary: suffix stripping and admission guards.
//!
//! The EMSE boundary extends the plain PUSH-immediate skeleton by also
//! stripping the Solidity CBOR metadata trailer and trailing zero bytes
//! before comparing bytecodes. Members of one EMSE family may therefore have
//! different raw lengths; callers must strip suffixes (`strip_emse_suffixes`)
//! before running PUSH-variance analysis or building data tables.
//!
//! Sharing one compiled artifact across members whose stripped suffixes
//! differ is only sound when execution can never reach the stripped region.
//! [`tail_can_fall_through`] (skeleton-level) and
//! [`stripped_suffix_has_jumpdest`] (member-level) are the conservative
//! admission guards enforcing that.

use revm_bytecode::opcode as op;
use revm_primitives::B256;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

/// g2 member filtering is off under the hardened interpreter semantics
/// (jumps into the stripped metadata region are invalid on BOTH sides, so a
/// metadata JUMPDEST can never be taken): the metadata boundary is reliably
/// identifiable today, so alignment replaces filtering. g1 stays on because
/// the code/data boundary inside the code region is NOT reliably
/// identifiable — only the reachability proof works there. Both become
/// unnecessary once EOF-style formats separate code/data/metadata. Flip to
/// true for vanilla yellow-paper jump semantics.
pub const ENFORCE_G2: bool = false;

/// Family-boundary selector for skeleton grouping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkeletonMode {
    /// PUSH-immediates-only skeleton over the full bytecode.
    Current,
    /// EMSE boundary: strip CBOR metadata + trailing zeros first.
    Emse,
}

impl SkeletonMode {
    /// Stable tag used in CLI flags, logs, and cache-file names.
    pub fn as_str(self) -> &'static str {
        match self {
            SkeletonMode::Current => "current",
            SkeletonMode::Emse => "emse",
        }
    }
}

impl core::str::FromStr for SkeletonMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "current" => Ok(SkeletonMode::Current),
            "emse" => Ok(SkeletonMode::Emse),
            other => Err(format!("unknown skeleton mode '{other}', expected current|emse")),
        }
    }
}

/// The bytecode view PUSH-variance analysis and data tables must be built
/// from under `mode`: the full bytecode for `Current`, the suffix-stripped
/// region for `Emse`. Callers strip first — never feed full bytecodes of an
/// EMSE family into `analyze_skeleton_group`.
pub fn bytecode_view(bytes: &[u8], mode: SkeletonMode) -> &[u8] {
    match mode {
        SkeletonMode::Current => bytes,
        SkeletonMode::Emse => strip_emse_suffixes(bytes),
    }
}

/// Skeleton bytes under `mode`; their hash is the family key.
pub fn skeleton_bytes(bytes: &[u8], mode: SkeletonMode) -> Vec<u8> {
    match mode {
        SkeletonMode::Current => crate::skeleton::extract_opcode_skeleton(bytes),
        SkeletonMode::Emse => extract_emse_skeleton(bytes),
    }
}

/// One admitted skeleton family (two or more members after guards).
#[derive(Clone, Debug)]
pub struct SkeletonFamily {
    /// Family key: hash of the mode's skeleton bytes (collision-split
    /// subgroups get a salted rehash).
    pub skel_hash: u64,
    /// Member code hashes, sorted for cross-run determinism.
    pub members: Vec<B256>,
}

/// Rejection / verification counters reported per corpus.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GroupingStats {
    /// Extra subgroups produced by skeleton-byte verification inside one
    /// hash bucket (0 unless a 64-bit hash collision actually occurred).
    pub collision_groups_split: usize,
    /// EMSE g1: whole skeletons rejected because their stripped region can
    /// fall through its physical end.
    pub g1_rejected_skeletons: usize,
    /// Members belonging to g1-rejected skeletons.
    pub g1_rejected_members: usize,
    /// EMSE g2: members rejected because their stripped suffix contains a
    /// valid JUMPDEST.
    pub g2_rejected_members: usize,
}

/// Result of [`group_skeleton_families`].
#[derive(Clone, Debug)]
pub struct SkeletonGrouping {
    /// Admitted families (>= 2 members), largest first.
    pub families: Vec<SkeletonFamily>,
    /// Everything else — singletons plus guard-rejected members — which must
    /// be compiled per-hash. Sorted.
    pub per_hash: Vec<B256>,
    /// Verification and guard counters.
    pub stats: GroupingStats,
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn salted_hash(bytes: &[u8], salt: u64) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    salt.hash(&mut hasher);
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// Split one hash bucket into subgroups of byte-identical skeletons.
/// Returns more than one subgroup only on a real 64-bit hash collision.
fn split_by_skeleton(members_with_skel: Vec<(B256, Vec<u8>)>) -> Vec<(Vec<u8>, Vec<B256>)> {
    let mut subgroups: Vec<(Vec<u8>, Vec<B256>)> = Vec::with_capacity(1);
    for (member, skel) in members_with_skel {
        match subgroups.iter_mut().find(|(s, _)| *s == skel) {
            Some((_, group)) => group.push(member),
            None => subgroups.push((skel, vec![member])),
        }
    }
    subgroups
}

/// Group `contracts` into skeleton families under `mode`.
///
/// The family key is the hash of the mode's skeleton bytes, verified by full
/// skeleton-byte equality within each hash bucket (colliding buckets are
/// split; split-off subgroups get salted rehashes so cache keys stay
/// unique). Under [`SkeletonMode::Emse`] the two admission guards run at
/// family-formation time: [`tail_can_fall_through`] rejects whole skeletons,
/// [`stripped_suffix_has_jumpdest`] rejects individual members. Rejected
/// members and singletons land in `per_hash`.
pub fn group_skeleton_families(
    contracts: &HashMap<B256, Vec<u8>>,
    mode: SkeletonMode,
) -> SkeletonGrouping {
    let mut stats = GroupingStats::default();
    // The skeleton is carried into the bucket rather than re-derived below:
    // re-deriving costs a second strip plus a fresh allocation per member.
    let mut buckets: HashMap<u64, Vec<(B256, Vec<u8>)>> = HashMap::new();
    for (code_hash, bytes) in contracts {
        let skel = skeleton_bytes(bytes, mode);
        buckets.entry(hash_bytes(&skel)).or_default().push((*code_hash, skel));
    }

    let mut used_hashes: HashSet<u64> = buckets.keys().copied().collect();
    let mut bucket_list: Vec<(u64, Vec<(B256, Vec<u8>)>)> = buckets.into_iter().collect();
    bucket_list.sort_by_key(|(h, _)| *h);

    let mut families = Vec::new();
    let mut per_hash = Vec::new();

    for (bucket_hash, mut with_skel) in bucket_list {
        with_skel.sort_by(|a, b| a.0.cmp(&b.0));
        if with_skel.len() == 1 {
            per_hash.push(with_skel[0].0);
            continue;
        }

        let mut subgroups = split_by_skeleton(with_skel);
        if subgroups.len() > 1 {
            stats.collision_groups_split += subgroups.len() - 1;
            subgroups.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
        }

        for (idx, (skel, mut group_members)) in subgroups.into_iter().enumerate() {
            let skel_hash = if idx == 0 {
                bucket_hash
            } else {
                let mut salt = idx as u64;
                loop {
                    let candidate = salted_hash(&skel, salt);
                    if used_hashes.insert(candidate) {
                        break candidate;
                    }
                    salt += 1;
                }
            };

            if mode == SkeletonMode::Emse {
                if tail_can_fall_through(&skel) {
                    stats.g1_rejected_skeletons += 1;
                    stats.g1_rejected_members += group_members.len();
                    per_hash.extend(group_members);
                    continue;
                }
                if ENFORCE_G2 {
                    group_members.retain(|m| {
                        let full = &contracts[m];
                        let stripped_len = strip_emse_suffixes(full).len();
                        if stripped_suffix_has_jumpdest(full, stripped_len) {
                            stats.g2_rejected_members += 1;
                            per_hash.push(*m);
                            false
                        } else {
                            true
                        }
                    });
                }
            }

            if group_members.len() >= 2 {
                families.push(SkeletonFamily { skel_hash, members: group_members });
            } else {
                per_hash.extend(group_members);
            }
        }
    }

    families.sort_by(|a, b| {
        b.members.len().cmp(&a.members.len()).then_with(|| a.skel_hash.cmp(&b.skel_hash))
    });
    per_hash.sort();
    SkeletonGrouping { families, per_hash, stats }
}

/// Extract an EMSE-style skeleton: strip the Solidity CBOR metadata trailer
/// and trailing zero bytes, then zero out every PUSH immediate byte while
/// preserving the stripped code length.
pub fn extract_emse_skeleton(bytes: &[u8]) -> Vec<u8> {
    let code = strip_emse_suffixes(bytes);
    let mut skeleton = Vec::with_capacity(code.len());
    let mut i = 0;
    while i < code.len() {
        let opcode = code[i];
        skeleton.push(opcode);
        i += 1;
        if (op::PUSH1..=op::PUSH32).contains(&opcode) {
            let n = (opcode - op::PUSH0) as usize;
            let take = n.min(code.len().saturating_sub(i));
            skeleton.extend(core::iter::repeat(0).take(take));
            i += take;
        }
    }
    skeleton
}

/// Strip the Solidity CBOR metadata trailer (if present) and trailing zero
/// bytes, returning the leading code region.
pub fn strip_emse_suffixes(bytes: &[u8]) -> &[u8] {
    strip_trailing_zeroes(strip_solidity_cbor_metadata(bytes))
}

fn strip_solidity_cbor_metadata(bytes: &[u8]) -> &[u8] {
    if bytes.len() < 2 {
        return bytes;
    }
    let metadata_len =
        u16::from_be_bytes([bytes[bytes.len() - 2], bytes[bytes.len() - 1]]) as usize;
    if metadata_len == 0 || bytes.len() < metadata_len + 2 {
        return bytes;
    }
    let metadata_start = bytes.len() - 2 - metadata_len;
    let metadata = &bytes[metadata_start..bytes.len() - 2];
    if looks_like_solidity_cbor_metadata(metadata) {
        &bytes[..metadata_start]
    } else {
        bytes
    }
}

fn strip_trailing_zeroes(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1] == 0 {
        end -= 1;
    }
    &bytes[..end]
}

fn looks_like_solidity_cbor_metadata(metadata: &[u8]) -> bool {
    if metadata.is_empty() {
        return false;
    }
    let first = metadata[0];
    let is_cbor_map = (first & 0xe0) == 0xa0 || first == 0xbf;
    is_cbor_map
        && ["ipfs", "bzzr0", "bzzr1", "solc", "experimental"]
            .iter()
            .any(|key| cbor_text_key_appears(metadata, key.as_bytes()))
}

fn cbor_text_key_appears(metadata: &[u8], key: &[u8]) -> bool {
    if key.len() >= 24 {
        return false;
    }
    let mut encoded = Vec::with_capacity(key.len() + 1);
    encoded.push(0x60 | key.len() as u8);
    encoded.extend_from_slice(key);
    metadata.windows(encoded.len()).any(|window| window == encoded.as_slice())
}

/// Admission guard g1 (skeleton-level): `true` if execution could reach the
/// physical end of the stripped code region and fall through into the
/// stripped suffix.
///
/// Reachability argument (no full CFG needed): control reaches a byte
/// position only via sequential flow or a jump. Sequential flow cannot pass
/// the last flow-blocking instruction (STOP / RETURN / REVERT / INVALID /
/// JUMP / SELFDESTRUCT), and a jump needs a valid JUMPDEST at an
/// instruction boundary. Hence the region after the last blocker — and with
/// it the physical end — is unreachable iff that region contains no
/// boundary JUMPDEST. Embedded data tails (e.g. Solidity revert strings)
/// therefore pass, unless the data happens to contain 0x5b ('[') at an
/// instruction boundary, in which case the EVM itself would accept a jump
/// there and we conservatively reject. No blocker at all (includes empty
/// input) is fall-through. A truncated trailing PUSH inside an unreachable
/// region is harmless (never executed); with no blocker before it, the
/// no-blocker rule already rejects.
pub fn tail_can_fall_through(code: &[u8]) -> bool {
    let mut saw_blocker = false;
    let mut jumpdest_after_blocker = false;
    let mut i = 0;
    while i < code.len() {
        let opcode = code[i];
        i += 1;
        if (op::PUSH1..=op::PUSH32).contains(&opcode) {
            i += (opcode - op::PUSH0) as usize;
        }
        match opcode {
            op::STOP | op::RETURN | op::REVERT | op::INVALID | op::JUMP | op::SELFDESTRUCT => {
                saw_blocker = true;
                jumpdest_after_blocker = false;
            }
            op::JUMPDEST => jumpdest_after_blocker = true,
            _ => {}
        }
    }
    !saw_blocker || jumpdest_after_blocker
}

/// Admission guard g2 (member-level): `true` if the member's stripped suffix
/// (`full[stripped_len..]`) contains a byte `0x5b` that the full bytecode's
/// jump analysis would mark as a valid JUMPDEST (i.e. at an instruction
/// boundary, not inside a PUSH immediate).
///
/// Such a member could legally be jumped into its stripped region, so it
/// must not execute under a skeleton compiled from the stripped view.
pub fn stripped_suffix_has_jumpdest(full: &[u8], stripped_len: usize) -> bool {
    let mut i = 0;
    while i < full.len() {
        let opcode = full[i];
        if i >= stripped_len && opcode == op::JUMPDEST {
            return true;
        }
        i += 1;
        if (op::PUSH1..=op::PUSH32).contains(&opcode) {
            i += (opcode - op::PUSH0) as usize;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_metadata(mut code: Vec<u8>, metadata: &[u8]) -> Vec<u8> {
        code.extend_from_slice(metadata);
        code.extend_from_slice(&(metadata.len() as u16).to_be_bytes());
        code
    }

    const SOLC_METADATA: [u8; 10] = [0xa1, 0x64, b's', b'o', b'l', b'c', 0x43, 0x00, 0x08, 0x11];

    #[test]
    fn strips_matching_solidity_cbor_metadata() {
        let bytecode = with_metadata(vec![0x60, 0x2a, 0x01], &SOLC_METADATA);
        assert_eq!(extract_emse_skeleton(&bytecode), vec![0x60, 0x00, 0x01]);
    }

    #[test]
    fn keeps_non_matching_metadata_like_tail() {
        let bytecode = with_metadata(vec![0x60, 0x2a, 0x01], &[0xa1, 0x63, b'f', b'o', b'o', 0x01]);
        let skeleton = extract_emse_skeleton(&bytecode);
        assert_eq!(&skeleton[..3], &[0x60, 0x00, 0x01]);
        assert_eq!(skeleton.len(), bytecode.len());
    }

    #[test]
    fn strips_trailing_zeroes_after_metadata_removal() {
        assert_eq!(extract_emse_skeleton(&[0x60, 0x2a, 0x01, 0x00, 0x00]), vec![0x60, 0x00, 0x01]);
    }

    #[test]
    fn strip_emse_suffixes_matches_skeleton_code_view() {
        let bytecode = with_metadata(vec![0x60, 0x2a, 0x01, 0x00, 0x00], &SOLC_METADATA);
        assert_eq!(strip_emse_suffixes(&bytecode), &[0x60, 0x2a, 0x01]);
    }

    #[test]
    fn metadata_length_differing_members_share_skeleton() {
        // Same logic prefix, metadata trailers of different lengths: the raw
        // bytecodes differ in length yet must yield identical EMSE skeletons.
        let logic = vec![0x60, 0x2a, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3];
        let long_metadata = [
            0xa2, 0x64, b'i', b'p', b'f', b's', 0x58, 0x22, 0x12, 0x20, 0x88, 0x99, 0x64, b's',
            b'o', b'l', b'c', 0x43, 0x00, 0x08, 0x1f,
        ];
        let a = with_metadata(logic.clone(), &SOLC_METADATA);
        let b = with_metadata(logic.clone(), &long_metadata);
        assert_ne!(a.len(), b.len());
        assert_eq!(extract_emse_skeleton(&a), extract_emse_skeleton(&b));
        assert_eq!(extract_emse_skeleton(&a), extract_emse_skeleton(&logic));
    }

    #[test]
    fn eip1167_minimal_proxy_variants_share_skeleton() {
        let proxy_a = hex_literal::hex!(
            "363d3d373d3d3d363d7311111111111111111111111111111111111111115af43d82803e903d91602b57fd5bf3"
        );
        let proxy_b = hex_literal::hex!(
            "363d3d373d3d3d363d7322222222222222222222222222222222222222225af43d82803e903d91602b57fd5bf3"
        );
        assert_eq!(extract_emse_skeleton(&proxy_a), extract_emse_skeleton(&proxy_b));
        assert_eq!(extract_emse_skeleton(&proxy_a).len(), 45);
    }

    #[test]
    fn fallthrough_guard_proves_data_tail_unreachable() {
        // REVERT blocks sequential flow; the embedded revert-string data
        // after it contains no boundary JUMPDEST, so the physical end is
        // provably unreachable — admissible even though scanning the data
        // ends in a truncated PUSH.
        let mut code = vec![0x60, 0x2a, op::REVERT];
        code.extend_from_slice(b"insufficient balance");
        assert!(!tail_can_fall_through(&code));
    }

    #[test]
    fn fallthrough_guard_rejects_jumpdest_after_last_blocker() {
        // A boundary 0x5b after the last blocker is a valid EVM jump target,
        // so the tail region stays reachable — reject.
        assert!(tail_can_fall_through(&[0x60, 0x2a, op::REVERT, 0x5b]));
        // '[' (0x5b) at an instruction boundary inside string data: same.
        let mut code = vec![0x60, 0x2a, op::REVERT];
        code.extend_from_slice(b"abc[def");
        assert!(tail_can_fall_through(&code));
    }

    #[test]
    fn fallthrough_guard_accepts_terminated_tails() {
        for terminator in [op::STOP, op::RETURN, op::REVERT, op::INVALID, op::JUMP, op::SELFDESTRUCT]
        {
            assert!(
                !tail_can_fall_through(&[0x60, 0x2a, terminator]),
                "terminator {terminator:#04x} must not be fall-through"
            );
        }
    }

    #[test]
    fn fallthrough_guard_rejects_open_tails() {
        // ADD at the end: execution runs off the stripped region.
        assert!(tail_can_fall_through(&[0x60, 0x2a, 0x01]));
        // JUMPI can fall through.
        assert!(tail_can_fall_through(&[0x60, 0x00, 0x60, 0x04, 0x57]));
        // Trailing PUSH with truncated immediate.
        assert!(tail_can_fall_through(&[0x61, 0x12]));
        // Empty region.
        assert!(tail_can_fall_through(&[]));
        // Trailing 0x00 inside a PUSH immediate is data, not STOP.
        assert!(tail_can_fall_through(&[0x60, 0x00]));
    }

    fn b256(byte: u8) -> B256 {
        B256::repeat_byte(byte)
    }

    fn contracts_of(entries: &[(B256, Vec<u8>)]) -> HashMap<B256, Vec<u8>> {
        entries.iter().cloned().collect()
    }

    // PUSH1 imm; PUSH1 0; MSTORE; PUSH1 32; PUSH1 0; RETURN — terminated tail.
    fn terminated_logic(imm: u8) -> Vec<u8> {
        vec![0x60, imm, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3]
    }

    #[test]
    fn metadata_length_differing_members_group_under_emse_only() {
        let long_metadata = [
            0xa2, 0x64, b'i', b'p', b'f', b's', 0x58, 0x22, 0x12, 0x20, 0x88, 0x99, 0x64, b's',
            b'o', b'l', b'c', 0x43, 0x00, 0x08, 0x1f,
        ];
        let a = with_metadata(terminated_logic(0x11), &SOLC_METADATA);
        let b = with_metadata(terminated_logic(0x22), &long_metadata);
        assert_ne!(a.len(), b.len());
        let contracts = contracts_of(&[(b256(1), a), (b256(2), b)]);

        let current = group_skeleton_families(&contracts, SkeletonMode::Current);
        assert!(current.families.is_empty(), "raw lengths differ; no family under Current");
        assert_eq!(current.per_hash.len(), 2);

        let emse = group_skeleton_families(&contracts, SkeletonMode::Emse);
        assert_eq!(emse.families.len(), 1);
        assert_eq!(emse.families[0].members, vec![b256(1), b256(2)]);
        assert!(emse.per_hash.is_empty());
        assert_eq!(emse.stats, GroupingStats::default());
    }

    #[test]
    fn g1_rejects_fallthrough_skeleton_families() {
        // Tail ends in ADD: falls through into the stripped metadata region.
        let open_logic = vec![0x60, 0x2a, 0x01];
        let a = with_metadata(open_logic.clone(), &SOLC_METADATA);
        let b = with_metadata(
            open_logic.clone(),
            &[
                0xa2, 0x64, b'i', b'p', b'f', b's', 0x58, 0x22, 0x12, 0x20, 0x88, 0x99, 0x64,
                b's', b'o', b'l', b'c', 0x43, 0x00, 0x08, 0x1f,
            ],
        );
        let contracts = contracts_of(&[(b256(1), a), (b256(2), b)]);
        let emse = group_skeleton_families(&contracts, SkeletonMode::Emse);
        assert!(emse.families.is_empty());
        assert_eq!(emse.per_hash.len(), 2);
        assert_eq!(emse.stats.g1_rejected_skeletons, 1);
        assert_eq!(emse.stats.g1_rejected_members, 2);
        assert_eq!(emse.stats.g2_rejected_members, 0);
    }

    #[test]
    fn g2_rejects_members_with_jumpdest_in_stripped_suffix() {
        // Poisoned trailer: still recognized as Solidity CBOR metadata, but
        // scanning past the logic tail lands on an instruction-boundary
        // JUMPDEST (0xa1 = LOG1, then 0x5b).
        let poisoned = [0xa1, 0x5b, 0x64, b's', b'o', b'l', b'c', 0x43, 0x00, 0x08, 0x11];
        let a = with_metadata(terminated_logic(0x11), &SOLC_METADATA);
        let b = with_metadata(terminated_logic(0x22), &SOLC_METADATA);
        let c = with_metadata(terminated_logic(0x33), &poisoned);
        assert!(stripped_suffix_has_jumpdest(
            &with_metadata(terminated_logic(0x33), &poisoned),
            terminated_logic(0x33).len()
        ));
        let contracts = contracts_of(&[(b256(1), a), (b256(2), b), (b256(3), c)]);
        let emse = group_skeleton_families(&contracts, SkeletonMode::Emse);
        assert_eq!(emse.families.len(), 1);
        if ENFORCE_G2 {
            assert_eq!(emse.families[0].members, vec![b256(1), b256(2)]);
            assert_eq!(emse.per_hash, vec![b256(3)]);
            assert_eq!(emse.stats.g2_rejected_members, 1);
        } else {
            // Hardened interpreter semantics: metadata JUMPDESTs can never be
            // taken on either side, so the poisoned member stays shareable.
            assert_eq!(emse.families[0].members, vec![b256(1), b256(2), b256(3)]);
            assert!(emse.per_hash.is_empty());
            assert_eq!(emse.stats.g2_rejected_members, 0);
        }
        assert_eq!(emse.stats.g1_rejected_skeletons, 0);
    }

    #[test]
    fn hash_bucket_collisions_split_by_skeleton_bytes() {
        // Real 64-bit collisions cannot be constructed here; exercise the
        // bucket-splitting helper directly with two distinct skeletons.
        let subgroups = split_by_skeleton(vec![
            (b256(1), vec![0x60, 0x00, 0xf3]),
            (b256(2), vec![0x60, 0x00, 0x00]),
            (b256(3), vec![0x60, 0x00, 0xf3]),
        ]);
        assert_eq!(subgroups.len(), 2);
        assert_eq!(subgroups[0].1, vec![b256(1), b256(3)]);
        assert_eq!(subgroups[1].1, vec![b256(2)]);
        // Salted rehash stays deterministic and differs from the plain hash.
        let skel = vec![0x60u8, 0x00, 0xf3];
        assert_ne!(salted_hash(&skel, 1), hash_bytes(&skel));
        assert_eq!(salted_hash(&skel, 1), salted_hash(&skel, 1));
    }

    #[test]
    fn emse_family_data_table_roundtrips_member_immediates() {
        use crate::skeleton::{analyze_skeleton_group, build_data_table, PushClassification};

        let long_metadata = [
            0xa2, 0x64, b'i', b'p', b'f', b's', 0x58, 0x22, 0x12, 0x20, 0x88, 0x99, 0x64, b's',
            b'o', b'l', b'c', 0x43, 0x00, 0x08, 0x1f,
        ];
        let a = with_metadata(terminated_logic(0x11), &SOLC_METADATA);
        let b = with_metadata(terminated_logic(0x22), &long_metadata);
        let contracts = contracts_of(&[(b256(1), a.clone()), (b256(2), b.clone())]);
        let emse = group_skeleton_families(&contracts, SkeletonMode::Emse);
        assert_eq!(emse.families.len(), 1);

        // Callers strip first: variance over stripped views, tables from
        // stripped members.
        let views: Vec<&[u8]> = emse.families[0]
            .members
            .iter()
            .map(|m| bytecode_view(&contracts[m], SkeletonMode::Emse))
            .collect();
        let variance = analyze_skeleton_group(&views);
        assert_eq!(variance.num_variant, 1, "only the first PUSH immediate differs");
        assert_eq!(variance.pushes[0], PushClassification::Variant { table_index: 0 });

        let table_a = build_data_table(bytecode_view(&a, SkeletonMode::Emse), &variance);
        let table_b = build_data_table(bytecode_view(&b, SkeletonMode::Emse), &variance);
        assert_eq!(table_a.data[0], 0x11);
        assert_eq!(table_b.data[0], 0x22);
        assert_eq!(table_a.data[1..32], [0u8; 31]);
    }

    #[test]
    fn jumpdest_guard_detects_real_jumpdest_in_suffix() {
        // STOP | JUMPDEST — the suffix byte is a valid JUMPDEST.
        assert!(stripped_suffix_has_jumpdest(&[0x00, 0x5b], 1));
        // PUSH1 0x5b — the suffix byte is immediate data, not a JUMPDEST.
        assert!(!stripped_suffix_has_jumpdest(&[0x60, 0x5b], 1));
        // 0x5b bytes covered by a PUSH4 immediate that starts before the
        // boundary must not count.
        assert!(!stripped_suffix_has_jumpdest(&[0x60, 0x00, 0x63, 0x5b, 0x5b, 0x5b, 0x5b, 0x00], 3));
        // ...but an instruction-boundary JUMPDEST after that immediate does.
        assert!(stripped_suffix_has_jumpdest(&[0x60, 0x00, 0x63, 0x5b, 0x5b, 0x5b, 0x5b, 0x5b], 3));
        // No suffix at all.
        assert!(!stripped_suffix_has_jumpdest(&[0x00, 0x5b], 2));
    }
}
