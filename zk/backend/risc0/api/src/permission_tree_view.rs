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
    pub levels: Vec<Vec<[u8; 32]>>,
}

impl PermissionTreeView {
    /// Builds a padded permission tree from a slice of exit leaves.
    ///
    /// Pads to `1 << required_depth(leaves.len())` with
    /// [`PermissionTreeAccumulator::hash_empty()`].
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
    pub fn root_with_leaf(&self, index: usize, leaf_hash: [u8; 32]) -> [u8; 32] {
        fold_path(leaf_hash, &self.siblings(index), index)
    }
}

/// Recomputes a tree root bottom-up from a leaf hash, sibling path, and leaf index.
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
        let tree = PermissionTreeView::from_leaves(&[l0, l1]);
        let s0 = tree.siblings(0);
        assert_eq!(fold_path(l0_hash, &s0, 0), tree.root());

        let new_leaf = [0x55u8; 32];
        assert_eq!(fold_path(new_leaf, &s0, 0), tree.root_with_leaf(0, new_leaf));
    }

    #[test]
    fn single_leaf_depth_zero_tree() {
        let leaf_hash = [0x42u8; 32];
        let tree = PermissionTreeView { levels: alloc::vec![alloc::vec![leaf_hash]] };

        assert_eq!(tree.depth(), 0);
        assert_eq!(tree.root(), leaf_hash);
        assert!(tree.siblings(0).is_empty());
        assert_eq!(fold_path(leaf_hash, &tree.siblings(0), 0), leaf_hash);
        assert_eq!(tree.root_with_leaf(0, leaf_hash), leaf_hash);
    }
}
