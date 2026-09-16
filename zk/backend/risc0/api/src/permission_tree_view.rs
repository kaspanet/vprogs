//! Padded permission-tree view over a list of exit leaves.
//!
//! Exposes a level-by-level view of the padded Merkle tree used for on-chain withdrawal
//! claims and spend tracking.

use alloc::{vec, vec::Vec};

use vprogs_zk_abi::withdrawal::ExitLeaf;

use crate::permission_tree::PermissionTreeAccumulator;

/// Padded permission-tree view over exit leaves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionTreeView {
    /// Tree levels from leaves (level 0) to root (last level).
    levels: Vec<Vec<[u8; 32]>>,
}

impl PermissionTreeView {
    /// Builds a padded permission tree from a slice of exit leaves.
    ///
    /// Always pads to `1 << required_depth(leaves.len())` with
    /// [`PermissionTreeAccumulator::hash_empty()`], matching the depth the on-chain redeem
    /// script embeds for the leaf count (`required_depth(1) == 1`: a single leaf pairs with
    /// the empty hash). For 0 leaves this is the all-empty depth-1 tree rooted at
    /// `hash_branch(empty, empty)`.
    pub fn from_leaves(leaves: &[ExitLeaf]) -> Self {
        let depth = PermissionTreeAccumulator::required_depth(leaves.len());
        let capacity = 1usize << depth;
        let empty = PermissionTreeAccumulator::hash_empty();
        let mut level0 = vec![empty; capacity];
        for (i, leaf) in leaves.iter().enumerate() {
            level0[i] = PermissionTreeAccumulator::hash_leaf(leaf.to_standard_spk(), leaf.amount);
        }
        let mut levels = vec![level0];
        for _ in 0..depth {
            let prev = levels.last().unwrap();
            let mut next = Vec::with_capacity(prev.len() / 2);
            for i in 0..prev.len() / 2 {
                next.push(PermissionTreeAccumulator::hash_branch(&prev[2 * i], &prev[2 * i + 1]));
            }
            levels.push(next);
        }
        Self { levels }
    }

    /// Root of the padded permission tree.
    pub fn root(&self) -> [u8; 32] {
        self.levels[self.depth()][0]
    }

    /// Depth of the tree (number of levels above the leaf level).
    pub fn depth(&self) -> usize {
        self.levels.len().saturating_sub(1)
    }

    /// Sibling hashes for the leaf at `index`.
    ///
    /// Requires `index < 1 << self.depth()` (panics if index is out of bounds).
    pub fn siblings(&self, index: usize) -> Vec<[u8; 32]> {
        let depth = self.depth();
        let mut out = Vec::with_capacity(depth);
        let mut idx = index;
        for level in 0..depth {
            out.push(self.levels[level][idx ^ 1]);
            idx /= 2;
        }
        out
    }

    /// Computes the new root if the leaf at `index` is replaced with `leaf_hash`.
    ///
    /// Requires `index < 1 << self.depth()`.
    pub fn root_with_leaf(&self, index: usize, leaf_hash: [u8; 32]) -> [u8; 32] {
        fold_path(leaf_hash, &self.siblings(index), index)
    }
}

/// Recomputes a tree root bottom-up from a leaf hash, sibling path, and leaf index.
///
/// Handles depth-0 paths (empty `siblings`) by returning `leaf_hash`.
pub fn fold_path(leaf_hash: [u8; 32], siblings: &[[u8; 32]], index: usize) -> [u8; 32] {
    let mut current = leaf_hash;
    for (level, sib) in siblings.iter().enumerate() {
        if (index >> level) & 1 == 0 {
            current = PermissionTreeAccumulator::hash_branch(&current, sib);
        } else {
            current = PermissionTreeAccumulator::hash_branch(sib, &current);
        }
    }
    current
}

#[cfg(test)]
mod tests {
    use vprogs_zk_abi::withdrawal::StandardSpk;

    use super::*;

    #[test]
    fn root_matches_manual_padded_root() {
        let pk0 = [0x11u8; 32];
        let pk1 = [0x22u8; 32];
        let l0 = ExitLeaf::from_pair(StandardSpk::PubKey(&pk0), 100);
        let l1 = ExitLeaf::from_pair(StandardSpk::PubKey(&pk1), 200);

        let h0 = PermissionTreeAccumulator::hash_leaf(l0.to_standard_spk(), l0.amount);
        let h1 = PermissionTreeAccumulator::hash_leaf(l1.to_standard_spk(), l1.amount);
        let manual_root = PermissionTreeAccumulator::hash_branch(&h0, &h1);

        let tree = PermissionTreeView::from_leaves(&[l0.clone(), l1.clone()]);
        assert_eq!(tree.depth(), PermissionTreeAccumulator::required_depth(2));
        assert_eq!(tree.root(), manual_root);

        // 3 leaves: depth 2, padded with hash_empty().
        let pk2 = [0x33u8; 32];
        let l2 = ExitLeaf::from_pair(StandardSpk::PubKey(&pk2), 300);
        let h2 = PermissionTreeAccumulator::hash_leaf(l2.to_standard_spk(), l2.amount);
        let tree3 = PermissionTreeView::from_leaves(&[l0, l1, l2]);
        let empty = PermissionTreeAccumulator::hash_empty();
        let b0 = PermissionTreeAccumulator::hash_branch(&h0, &h1);
        let b1 = PermissionTreeAccumulator::hash_branch(&h2, &empty);
        let manual3_root = PermissionTreeAccumulator::hash_branch(&b0, &b1);
        assert_eq!(tree3.depth(), PermissionTreeAccumulator::required_depth(3));
        assert_eq!(tree3.root(), manual3_root);
    }

    #[test]
    fn siblings_differ_at_every_level() {
        let pk0 = [0x11u8; 32];
        let pk1 = [0x22u8; 32];
        let l0 = ExitLeaf::from_pair(StandardSpk::PubKey(&pk0), 100);
        let l1 = ExitLeaf::from_pair(StandardSpk::PubKey(&pk1), 200);
        let tree = PermissionTreeView::from_leaves(&[l0, l1]);

        let s0 = tree.siblings(0);
        let s1 = tree.siblings(1);
        assert!(!s0.is_empty());
        assert_eq!(s0.len(), s1.len());
        for (sib0, sib1) in s0.iter().zip(s1.iter()) {
            assert_ne!(sib0, sib1);
        }
    }

    #[test]
    fn fold_path_reproduces_root_and_root_with_leaf() {
        let pk0 = [0x11u8; 32];
        let pk1 = [0x22u8; 32];
        let l0 = ExitLeaf::from_pair(StandardSpk::PubKey(&pk0), 100);
        let l1 = ExitLeaf::from_pair(StandardSpk::PubKey(&pk1), 200);

        let l0_hash = PermissionTreeAccumulator::hash_leaf(l0.to_standard_spk(), l0.amount);
        let l1_hash = PermissionTreeAccumulator::hash_leaf(l1.to_standard_spk(), l1.amount);
        let tree = PermissionTreeView::from_leaves(&[l0, l1]);
        let s0 = tree.siblings(0);
        assert_eq!(fold_path(l0_hash, &s0, 0), tree.root());
        let s1 = tree.siblings(1);
        assert_eq!(fold_path(l1_hash, &s1, 1), tree.root());

        let new_leaf = [0x55u8; 32];
        assert_eq!(fold_path(new_leaf, &s0, 0), tree.root_with_leaf(0, new_leaf));
    }

    #[test]
    fn single_leaf_pairs_with_empty_sibling() {
        let pk0 = [0x11u8; 32];
        let l0 = ExitLeaf::from_pair(StandardSpk::PubKey(&pk0), 100);
        let l0_hash = PermissionTreeAccumulator::hash_leaf(l0.to_standard_spk(), l0.amount);
        let empty = PermissionTreeAccumulator::hash_empty();
        let tree = PermissionTreeView::from_leaves(&[l0]);

        // The on-chain redeem for 1 leaf embeds depth 1: leaf paired with the empty hash.
        assert_eq!(tree.depth(), PermissionTreeAccumulator::required_depth(1));
        assert_eq!(tree.depth(), 1);
        assert_eq!(tree.root(), PermissionTreeAccumulator::hash_branch(&l0_hash, &empty));
        assert_eq!(tree.siblings(0), vec![empty]);
        assert_eq!(fold_path(l0_hash, &tree.siblings(0), 0), tree.root());
        assert_eq!(tree.root_with_leaf(0, l0_hash), tree.root());
    }

    #[test]
    fn view_roots_match_accumulator_padded_roots() {
        // The view must reproduce the accumulator's padded root (the value the on-chain
        // redeem embeds) for every leaf count, including the single-leaf depth-1 fold.
        for k in 1..=5usize {
            let leaves: Vec<_> = (0..k)
                .map(|i| {
                    ExitLeaf::from_pair(StandardSpk::PubKey(&[i as u8 + 1; 32]), 100 + i as u64)
                })
                .collect();
            let mut acc = PermissionTreeAccumulator::new();
            for leaf in &leaves {
                acc.add_exit(leaf.to_standard_spk(), leaf.amount);
            }
            let view = PermissionTreeView::from_leaves(&leaves);
            assert_eq!(view.depth(), PermissionTreeAccumulator::required_depth(k));
            assert_eq!(view.root(), acc.root());
        }
    }
}
