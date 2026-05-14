//! Permission (exit) Merkle tree — SHA-256 tree of `(spk, amount)` leaves.
//!
//! Provides:
//! - Pure tree primitives: leaf/branch hashes, the build-time [`PERM_EMPTY_HASHES`] table, depth
//!   math, streaming builder.
//! - [`PermissionTreeAccumulator`], the default [`ExitAccumulator`] that hashes leaves with the
//!   on-chain SPK byte representation of each [`StandardSpk`] and commits the bundle's exits.
//!
//! ## What the 32-byte commitment is
//!
//! `PermissionTreeAccumulator::finalize` wraps the padded tree root in the permission redeem
//! script (see [`crate::permission_script`]) and returns `blake2b(redeem)` — the P2SH
//! script-hash. The settlement exit output's SPK is `pay_to_script_hash` of this value, so
//! [`StateTransition::permission_spk_hash`] is directly checkable on-chain. `finalize` returns
//! `[0u8; 32]` when no exits were emitted, keeping settlement in single-output mode.
//!
//! [`StandardSpk`]: vprogs_zk_abi::transaction_processor::StandardSpk
//! [`ExitAccumulator`]: vprogs_zk_abi::batch_processor::ExitAccumulator
//! [`StateTransition::permission_spk_hash`]: vprogs_zk_abi::batch_processor::StateTransition::permission_spk_hash

use sha2::Digest;
use vprogs_zk_abi::{batch_processor::ExitAccumulator, transaction_processor::StandardSpk};

use crate::permission_script::{blake2b_script_hash, build_permission_redeem_script};

/// Maximum permission-tree depth.
///
/// [`PermissionTreeAccumulator`] is bundle-wide and counts leaves in a `u32`, so the deepest
/// tree it can build has `u32::MAX` leaves — `required_depth(u32::MAX)` is 32 levels. Sizing
/// the depth (and the streaming builder's stack) to that bound keeps `required_depth`'s clamp
/// from ever being lossy and `add_leaf` from ever overflowing its stack, for *any* `u32` leaf
/// count.
pub const PERM_MAX_DEPTH: usize = 32;

const PERM_LEAF_DOMAIN: &[u8; 8] = b"PermLeaf";
const PERM_BRANCH_DOMAIN: &[u8; 10] = b"PermBranch";

/// Empty-subtree hashes for permission-tree levels `0..=PERM_MAX_DEPTH`, precomputed at build
/// time (see `build.rs`). `[0]` is `sha256("PermEmpty")` — the empty leaf; `[L]` is
/// `sha256("PermBranch" || [L-1] || [L-1])` — the root of an all-empty subtree of height `L`.
/// Computing these in the guest costs one SHA-256 per level; the table makes it an array read.
/// Mirrors `kaspa-smt`'s `EMPTY_HASHES`. `empty_hashes_table_matches_runtime` revalidates it.
pub(crate) const PERM_EMPTY_HASHES: [[u8; 32]; PERM_MAX_DEPTH + 1] =
    include!(concat!(env!("OUT_DIR"), "/perm_empty_hashes_generated.rs"));

/// Streaming-builder stack depth. The builder holds one entry per distinct tree level, i.e.
/// `popcount(leaf_count)` entries; for a `u32` leaf count that peaks at 32 — exactly
/// [`PERM_MAX_DEPTH`].
const MAX_STREAM_STACK: usize = PERM_MAX_DEPTH;

/// SHA-256 leaf hash: `sha256("PermLeaf" || spk_bytes || amount_le)`.
pub fn perm_leaf_hash(spk: &[u8], amount: u64) -> [u8; 32] {
    let mut hasher = sha2::Sha256::new_with_prefix(PERM_LEAF_DOMAIN);
    hasher.update(spk);
    hasher.update(amount.to_le_bytes());
    hasher.finalize().into()
}

/// SHA-256 branch hash: `sha256("PermBranch" || left || right)`.
pub fn perm_branch_hash(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut hasher = sha2::Sha256::new_with_prefix(PERM_BRANCH_DOMAIN);
    hasher.update(left);
    hasher.update(right);
    hasher.finalize().into()
}

/// Required depth for `count` leaves: `ceil(log2(count))`, clamped to `[1, PERM_MAX_DEPTH]`.
pub fn required_depth(count: usize) -> usize {
    if count <= 1 {
        return 1;
    }
    let bits = usize::BITS - (count - 1).leading_zeros();
    (bits as usize).min(PERM_MAX_DEPTH)
}

/// Streaming permission-tree builder — stack of `(level, hash)` pairs, no heap allocation.
///
/// Append leaves with [`add_leaf`] and call [`finalize`] to extract the root at the builder's
/// natural depth — `required_depth(leaf_count)` levels.
///
/// [`add_leaf`]: StreamingPermTreeBuilder::add_leaf
/// [`finalize`]: StreamingPermTreeBuilder::finalize
pub struct StreamingPermTreeBuilder {
    stack: [(u32, [u8; 32]); MAX_STREAM_STACK],
    stack_len: usize,
    leaf_count: u32,
}

impl Default for StreamingPermTreeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamingPermTreeBuilder {
    /// Creates an empty builder.
    pub fn new() -> Self {
        Self { stack: [(0, [0u8; 32]); MAX_STREAM_STACK], stack_len: 0, leaf_count: 0 }
    }

    /// Appends a leaf to the tree.
    pub fn add_leaf(&mut self, hash: [u8; 32]) {
        let mut level = 0u32;
        let mut current = hash;

        while self.stack_len > 0 {
            let (top_level, top_hash) = self.stack[self.stack_len - 1];
            if top_level != level {
                break;
            }
            self.stack_len -= 1;
            current = perm_branch_hash(&top_hash, &current);
            level += 1;
        }

        self.stack[self.stack_len] = (level, current);
        self.stack_len += 1;
        self.leaf_count += 1;
    }

    /// Returns the number of leaves added so far.
    pub fn leaf_count(&self) -> u32 {
        self.leaf_count
    }

    /// Finalizes the tree, padding incomplete subtrees with the empty-subtree hash.
    pub fn finalize(&self) -> [u8; 32] {
        if self.leaf_count == 0 {
            return PERM_EMPTY_HASHES[0];
        }

        if self.leaf_count == 1 {
            return perm_branch_hash(&self.stack[0].1, &PERM_EMPTY_HASHES[0]);
        }

        let mut result_hash = [0u8; 32];
        let mut result_level = 0u32;
        let mut first = true;

        for i in (0..self.stack_len).rev() {
            let (level, hash) = self.stack[i];

            if first {
                result_hash = hash;
                result_level = level;
                first = false;
                continue;
            }

            while result_level < level {
                result_hash =
                    perm_branch_hash(&result_hash, &PERM_EMPTY_HASHES[result_level as usize]);
                result_level += 1;
            }

            result_hash = perm_branch_hash(&hash, &result_hash);
            result_level += 1;
        }

        result_hash
    }
}

/// Default [`ExitAccumulator`] — builds a SHA-256 permission tree over the bundle's exits and
/// returns the permission redeem script's P2SH script-hash as the 32-byte commitment.
///
/// Hashes each leaf as `sha256("PermLeaf" || spk_script_bytes || amount_le)` where
/// `spk_script_bytes` is the on-chain Kaspa script of the destination (e.g. 34 bytes for a
/// Schnorr P2PK, 35 bytes for ECDSA / P2SH). See [`StandardSpk::to_script_bytes`].
pub struct PermissionTreeAccumulator {
    builder: StreamingPermTreeBuilder,
}

impl Default for PermissionTreeAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

impl PermissionTreeAccumulator {
    /// Creates an empty accumulator.
    pub fn new() -> Self {
        Self { builder: StreamingPermTreeBuilder::new() }
    }
}

impl ExitAccumulator for PermissionTreeAccumulator {
    fn add_exit(&mut self, dest: StandardSpk<'_>, amount: u64) {
        let spk_bytes = dest.to_script_bytes();
        self.builder.add_leaf(perm_leaf_hash(spk_bytes.as_slice(), amount));
    }

    fn finalize(&self) -> [u8; 32] {
        let count = self.builder.leaf_count();
        if count == 0 {
            return [0u8; 32];
        }
        let depth = required_depth(count as usize);
        // `builder.finalize` already returns the root at exactly `required_depth(count)` levels.
        let root = self.builder.finalize();
        // Wrap the root in the permission redeem script and return its P2SH script-hash: the
        // settlement exit output's SPK is `pay_to_script_hash` of this value.
        let redeem = build_permission_redeem_script(&root, count as u64, depth);
        blake2b_script_hash(&redeem)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_depth_clamps() {
        assert_eq!(required_depth(0), 1);
        assert_eq!(required_depth(1), 1);
        assert_eq!(required_depth(2), 1);
        assert_eq!(required_depth(3), 2);
        assert_eq!(required_depth(4), 2);
        assert_eq!(required_depth(5), 3);
        assert_eq!(required_depth(256), 8);
        assert_eq!(required_depth(1 << PERM_MAX_DEPTH), PERM_MAX_DEPTH);
        assert_eq!(required_depth((1 << PERM_MAX_DEPTH) + 1), PERM_MAX_DEPTH);
        assert_eq!(required_depth(usize::MAX), PERM_MAX_DEPTH);
    }

    #[test]
    fn finalize_three_leaves_matches_manual_padded_root() {
        // Exercises the multi-entry combine and the `while result_level < level` padding loop
        // that the power-of-2 counts (1, 2) never reach.
        let h: [[u8; 32]; 3] = [[0x01; 32], [0x02; 32], [0x03; 32]];
        let mut builder = StreamingPermTreeBuilder::new();
        for leaf in &h {
            builder.add_leaf(*leaf);
        }
        assert_eq!(required_depth(3), 2);

        // depth-2 padded tree over leaves [h0, h1, h2, empty].
        let b01 = perm_branch_hash(&h[0], &h[1]);
        let b2e = perm_branch_hash(&h[2], &PERM_EMPTY_HASHES[0]);
        let root = perm_branch_hash(&b01, &b2e);
        assert_eq!(builder.finalize(), root);
    }

    #[test]
    fn finalize_five_leaves_matches_manual_padded_root() {
        // The right subtree pads two levels deep, so the padding loop runs more than once.
        let h: [[u8; 32]; 5] = [[0x01; 32], [0x02; 32], [0x03; 32], [0x04; 32], [0x05; 32]];
        let mut builder = StreamingPermTreeBuilder::new();
        for leaf in &h {
            builder.add_leaf(*leaf);
        }
        assert_eq!(required_depth(5), 3);

        // depth-3 padded tree over leaves [h0, h1, h2, h3, h4, empty, empty, empty].
        let left =
            perm_branch_hash(&perm_branch_hash(&h[0], &h[1]), &perm_branch_hash(&h[2], &h[3]));
        let right = perm_branch_hash(
            &perm_branch_hash(&h[4], &PERM_EMPTY_HASHES[0]),
            &PERM_EMPTY_HASHES[1],
        );
        let root = perm_branch_hash(&left, &right);
        assert_eq!(builder.finalize(), root);
    }

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

        // Determinism — finalize is &self, can be called repeatedly.
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
        let spk_a = StandardSpk::PubKey(&pk_a).to_script_bytes();
        let spk_b = StandardSpk::PubKey(&pk_b).to_script_bytes();
        let leaf_a = perm_leaf_hash(spk_a.as_slice(), 100);
        let leaf_b = perm_leaf_hash(spk_b.as_slice(), 200);
        let manual_root = perm_branch_hash(&leaf_a, &leaf_b);
        let depth = required_depth(2);
        let redeem = build_permission_redeem_script(&manual_root, 2, depth);
        assert_eq!(commitment, blake2b_script_hash(&redeem));
    }

    #[test]
    fn empty_hashes_table_matches_runtime() {
        // Revalidate the build.rs-generated table against a runtime recomputation: level 0 is
        // `sha256("PermEmpty")`, each level up is `perm_branch_hash` of the level below itself.
        let mut expected: [u8; 32] = sha2::Sha256::new_with_prefix(b"PermEmpty").finalize().into();
        assert_eq!(PERM_EMPTY_HASHES[0], expected, "level 0");
        for (level, &table_hash) in PERM_EMPTY_HASHES.iter().enumerate().skip(1) {
            expected = perm_branch_hash(&expected, &expected);
            assert_eq!(table_hash, expected, "level {level}");
        }
    }
}
