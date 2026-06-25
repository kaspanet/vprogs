//! Permission (exit) Merkle tree of `(spk, amount)` leaves.
//!
//! Uses the generic [`StreamingBuilder`](vprogs_core_merkle_tree::StreamingBuilder) over SHA-256
//! with the 1-byte [`PermTags`](crate::permission_tags::PermTags) enum domain shared with the
//! on-chain redeem script (issue #78). Holds the build-time empty-subtree hash table and exposes
//! [`PermissionTreeAccumulator`], the default [`ExitAccumulator`] that hashes leaves with the
//! on-chain SPK byte representation of each [`StandardSpk`] and commits the bundle's exits.
//!
//! ## On-chain commitment
//!
//! [`PermissionTreeAccumulator::finalize`]'s return value lands in
//! [`StateTransition::permission_spk_hash`], where it's checked on-chain against the settlement
//! exit output's SPK via `pay_to_script_hash`. The `[0u8; 32]` result for an empty bundle keeps
//! settlement in single-output mode (no exit output needed).
//!
//! [`StateTransition::permission_spk_hash`]: vprogs_zk_abi::batch_aggregator::StateTransition::permission_spk_hash

use vprogs_zk_abi::withdrawal::{ExitAccumulator, StandardSpk};

use crate::{
    permission_script::{blake2b_script_hash, build_permission_redeem_script},
    permission_tags::PermTags,
};

/// Permission-tree streaming builder: SHA-256, 1-byte [`PermTags`] domain, depth 32.
///
/// Depth 32 covers `u32::MAX` leaves (the accumulator's count type), keeping `required_depth`'s
/// clamp non-lossy and the stack non-overflowing for any input.
type Builder = vprogs_core_merkle_tree::StreamingBuilder<crate::Sha256, PermTags, 32, 1>;

/// Default [`ExitAccumulator`]: accumulates the bundle's exits into a permission tree and
/// finalizes to the on-chain commitment.
#[derive(Default)]
pub struct PermissionTreeAccumulator {
    builder: Builder,
}

impl PermissionTreeAccumulator {
    /// Maximum tree depth.
    pub const MAX_DEPTH: usize = Builder::MAX_DEPTH;

    /// The permission tree's empty-subtree hash table, precomputed at build time (see `build.rs`)
    /// by [`Builder::compute_empty_hashes`] to avoid recomputing it at runtime.
    pub const EMPTY_HASHES: [[u8; 32]; Self::MAX_DEPTH] =
        include!(concat!(env!("OUT_DIR"), "/perm_empty_hashes_generated.rs"));

    /// Creates an empty accumulator.
    pub fn new() -> Self {
        Self { builder: Builder::new() }
    }

    /// Adds an exit to the tree.
    pub fn add_exit(&mut self, dest: StandardSpk<'_>, amount: u64) {
        self.builder.add_leaf_parts([dest.to_script_bytes().as_slice(), &amount.to_le_bytes()]);
    }

    /// Returns the permission tree's P2SH script-hash, or `[0u8; 32]` when no exits were added.
    pub fn finalize(&self) -> [u8; 32] {
        // Return early if no exits were added.
        let count = self.builder.leaf_count();
        if count == 0 {
            return [0u8; 32];
        }

        // Compute root of padded merkle tree.
        let root = self.builder.finalize(&Self::EMPTY_HASHES);

        // Wrap in the permission redeem script and return its P2SH script-hash.
        let depth = Builder::required_depth(count as usize);
        let redeem = build_permission_redeem_script(&root, count as u64, depth);
        blake2b_script_hash(&redeem)
    }

    /// Leaf hash for an exit `(dest, amount)`:
    /// `hash(LEAF_TAG || dest.to_script_bytes() || amount.to_le_bytes())`.
    pub fn hash_leaf(dest: StandardSpk<'_>, amount: u64) -> [u8; 32] {
        Builder::hash_leaf_parts([dest.to_script_bytes().as_slice(), &amount.to_le_bytes()])
    }

    /// Branch hash: `hash(BRANCH_TAG || left || right)`.
    pub fn hash_branch(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
        Builder::hash_branch(left, right)
    }

    /// Empty-subtree hash: `hash(EMPTY_TAG)`.
    pub fn hash_empty() -> [u8; 32] {
        Self::EMPTY_HASHES[0]
    }

    /// Required tree depth to hold `count` exits.
    pub fn required_depth(count: usize) -> usize {
        Builder::required_depth(count)
    }
}

impl ExitAccumulator for PermissionTreeAccumulator {
    fn add_exit(&mut self, dest: StandardSpk<'_>, amount: u64) {
        self.add_exit(dest, amount)
    }

    fn finalize(&self) -> [u8; 32] {
        self.finalize()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission_tags::PermNode;

    #[test]
    fn empty_accumulator_finalize_is_zero() {
        let acc = PermissionTreeAccumulator::new();
        assert_eq!(acc.finalize(), [0u8; 32]);
    }

    #[test]
    fn accumulator_single_exit() {
        let mut acc = PermissionTreeAccumulator::new();
        acc.add_exit(StandardSpk::PubKey(&[0xAA; 32]), 1000);
        let commitment = acc.finalize();
        assert_ne!(commitment, [0u8; 32]);

        // Determinism, finalize is &self, can be called repeatedly.
        assert_eq!(acc.finalize(), commitment);
    }

    #[test]
    fn accumulator_two_exits_matches_manual() {
        let pk_a = [0x11u8; 32];
        let pk_b = [0x22u8; 32];
        let mut acc = PermissionTreeAccumulator::new();
        acc.add_exit(StandardSpk::PubKey(&pk_a), 100);
        acc.add_exit(StandardSpk::PubKey(&pk_b), 200);
        let commitment = acc.finalize();

        // Manual: leaf hashes → tree root → permission redeem script → P2SH script-hash.
        let leaf_a = PermissionTreeAccumulator::hash_leaf(StandardSpk::PubKey(&pk_a), 100);
        let leaf_b = PermissionTreeAccumulator::hash_leaf(StandardSpk::PubKey(&pk_b), 200);
        let manual_root = PermissionTreeAccumulator::hash_branch(&leaf_a, &leaf_b);
        let depth = PermissionTreeAccumulator::required_depth(2);
        let redeem = build_permission_redeem_script(&manual_root, 2, depth);
        assert_eq!(commitment, blake2b_script_hash(&redeem));
    }

    #[test]
    fn hashes_match_script_tag_domains() {
        // Issue #78: the off-chain accumulator must hash leaf/branch/empty with the SAME 1-byte tag
        // domains the on-chain redeem script uses (`permission_script.rs` pushes
        // `[PermNode::Leaf]`, `[PermNode::Branch]`, `[PermNode::Empty]` before OP_SHA256).
        // Recompute each independently and assert byte equality, so a domain regression
        // here would fail before reaching the engine.
        use sha2::{Digest, Sha256 as RefSha256};

        let pk = [0x33u8; 32];
        let spk = StandardSpk::PubKey(&pk);
        let amount = 4321u64;

        // Leaf: SHA256(Leaf || spk_bytes || amount_le).
        let mut h = RefSha256::new();
        h.update([PermNode::Leaf as u8]);
        h.update(spk.to_script_bytes().as_slice());
        h.update(amount.to_le_bytes());
        let expected_leaf: [u8; 32] = h.finalize().into();
        assert_eq!(PermissionTreeAccumulator::hash_leaf(spk, amount), expected_leaf);

        // Branch: SHA256(Branch || left || right).
        let left = [0x01u8; 32];
        let right = [0x02u8; 32];
        let mut h = RefSha256::new();
        h.update([PermNode::Branch as u8]);
        h.update(left);
        h.update(right);
        let expected_branch: [u8; 32] = h.finalize().into();
        assert_eq!(PermissionTreeAccumulator::hash_branch(&left, &right), expected_branch);

        // Empty: SHA256(Empty).
        let expected_empty: [u8; 32] = RefSha256::digest([PermNode::Empty as u8]).into();
        assert_eq!(PermissionTreeAccumulator::hash_empty(), expected_empty);
    }

    #[test]
    fn root_matches_script_side_recomputation() {
        // Issue #78 test 2: the accumulator's root for a known leaf set must equal the root
        // recomputed via the script-side tag-domain hashing for the same leaves.
        use sha2::{Digest, Sha256 as RefSha256};

        fn ref_leaf(spk: &[u8], amount: u64) -> [u8; 32] {
            let mut h = RefSha256::new();
            h.update([PermNode::Leaf as u8]);
            h.update(spk);
            h.update(amount.to_le_bytes());
            h.finalize().into()
        }
        fn ref_branch(l: &[u8; 32], r: &[u8; 32]) -> [u8; 32] {
            let mut h = RefSha256::new();
            h.update([PermNode::Branch as u8]);
            h.update(l);
            h.update(r);
            h.finalize().into()
        }

        let pk_a = [0xAAu8; 32];
        let pk_b = [0xBBu8; 32];
        let pk_c = [0xCCu8; 32];

        let mut acc = PermissionTreeAccumulator::new();
        acc.add_exit(StandardSpk::PubKey(&pk_a), 100);
        acc.add_exit(StandardSpk::PubKey(&pk_b), 200);
        acc.add_exit(StandardSpk::PubKey(&pk_c), 300);
        // Accumulator root (depth 2 over [a, b, c, empty]).
        let acc_root = acc.builder.finalize(&PermissionTreeAccumulator::EMPTY_HASHES);

        let la = ref_leaf(StandardSpk::PubKey(&pk_a).to_script_bytes().as_slice(), 100);
        let lb = ref_leaf(StandardSpk::PubKey(&pk_b).to_script_bytes().as_slice(), 200);
        let lc = ref_leaf(StandardSpk::PubKey(&pk_c).to_script_bytes().as_slice(), 300);
        let empty = RefSha256::digest([PermNode::Empty as u8]).into();
        let b01 = ref_branch(&la, &lb);
        let b2e = ref_branch(&lc, &empty);
        let expected_root = ref_branch(&b01, &b2e);

        assert_eq!(acc_root, expected_root);
    }

    #[test]
    fn empty_hashes_table_matches_build_script() {
        // Catches drift between the build.rs-generated table and what `compute_empty_hashes`
        // would produce at runtime for the same (hasher, tags, depth) configuration.
        assert_eq!(PermissionTreeAccumulator::EMPTY_HASHES, Builder::compute_empty_hashes());
    }
}
