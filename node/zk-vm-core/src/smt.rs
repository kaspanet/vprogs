//! Blake3 Sparse Merkle Tree.
//!
//! A 16-level SMT (65536 leaves) using blake3 domain-separated hashing.
//! Shared between host and guest.

use crate::hashing::domain_to_key;

/// Tree depth: 16 levels = 65536 possible leaves.
pub const SMT_DEPTH: usize = 16;

const LEAF_DOMAIN: &[u8] = b"SMTLeaf";
const EMPTY_DOMAIN: &[u8] = b"SMTEmpty";
const BRANCH_DOMAIN: &[u8] = b"SMTBranch";

const LEAF_KEY: [u8; blake3::KEY_LEN] = domain_to_key(LEAF_DOMAIN);
const EMPTY_KEY: [u8; blake3::KEY_LEN] = domain_to_key(EMPTY_DOMAIN);
const BRANCH_KEY: [u8; blake3::KEY_LEN] = domain_to_key(BRANCH_DOMAIN);

/// Hash of an empty leaf.
pub fn empty_leaf_hash() -> [u8; 32] {
    *blake3::keyed_hash(&EMPTY_KEY, &[]).as_bytes()
}

/// Hash a leaf: `blake3_keyed("SMTLeaf", resource_id_hash || state_hash)`.
pub fn leaf_hash(resource_id_hash: &[u8; 32], state_hash: &[u8; 32]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_keyed(&LEAF_KEY);
    hasher.update(resource_id_hash);
    hasher.update(state_hash);
    *hasher.finalize().as_bytes()
}

/// Hash a branch: `blake3_keyed("SMTBranch", left || right)`.
pub fn branch_hash(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_keyed(&BRANCH_KEY);
    hasher.update(left);
    hasher.update(right);
    *hasher.finalize().as_bytes()
}

/// Map a resource_id_hash to its leaf index (uses first 2 bytes = u16).
pub fn key_to_index(resource_id_hash: &[u8; 32]) -> u16 {
    u16::from_le_bytes([resource_id_hash[0], resource_id_hash[1]])
}

/// SMT proof structure for a 16-level tree.
///
/// Contains sibling hashes at each level from leaf to root.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SmtProof {
    pub siblings: [[u8; 32]; SMT_DEPTH],
}

impl SmtProof {
    /// Create an empty proof (all siblings are empty subtree hashes).
    pub fn empty() -> Self {
        let mut siblings = [[0u8; 32]; SMT_DEPTH];
        let mut current = empty_leaf_hash();
        for sibling in &mut siblings {
            *sibling = current;
            current = branch_hash(&current, &current);
        }
        Self { siblings }
    }

    /// Compute the root given a leaf hash and key index.
    pub fn compute_root(&self, leaf: &[u8; 32], key: u16) -> [u8; 32] {
        let mut current = *leaf;
        for (level, sibling) in self.siblings.iter().enumerate() {
            let bit = (key >> level) & 1;
            if bit == 0 {
                current = branch_hash(&current, sibling);
            } else {
                current = branch_hash(sibling, &current);
            }
        }
        current
    }

    /// Verify that a leaf with given hash exists at key under given root.
    pub fn verify(&self, root: &[u8; 32], key: u16, leaf: &[u8; 32]) -> bool {
        self.compute_root(leaf, key) == *root
    }
}

impl Default for SmtProof {
    fn default() -> Self {
        Self::empty()
    }
}

/// In-memory Sparse Merkle Tree for the host (prover side).
#[cfg(feature = "std")]
pub struct Smt {
    /// Leaf values: (resource_id_hash, state_hash) or None for empty.
    leaves: alloc::vec::Vec<Option<([u8; 32], [u8; 32])>>,
}

#[cfg(feature = "std")]
impl Default for Smt {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "std")]
impl Smt {
    const NUM_LEAVES: usize = 1 << SMT_DEPTH;

    /// Create a new empty SMT.
    pub fn new() -> Self {
        Self { leaves: alloc::vec![None; Self::NUM_LEAVES] }
    }

    /// Compute the hash of an empty subtree at the given level.
    fn empty_subtree_hash(level: usize) -> [u8; 32] {
        let mut current = empty_leaf_hash();
        for _ in 0..level {
            current = branch_hash(&current, &current);
        }
        current
    }

    /// Insert or update a resource's state.
    pub fn upsert(&mut self, resource_id_hash: [u8; 32], state_hash: [u8; 32]) -> bool {
        let key = key_to_index(&resource_id_hash) as usize;
        let was_update = self.leaves[key].is_some();
        self.leaves[key] = Some((resource_id_hash, state_hash));
        was_update
    }

    /// Get the state hash at a key index.
    pub fn get(&self, resource_id_hash: &[u8; 32]) -> Option<[u8; 32]> {
        let key = key_to_index(resource_id_hash) as usize;
        self.leaves[key].as_ref().and_then(
            |(rid, sh)| {
                if rid == resource_id_hash { Some(*sh) } else { None }
            },
        )
    }

    /// Compute the root hash.
    pub fn root(&self) -> [u8; 32] {
        self.compute_node(SMT_DEPTH, 0)
    }

    fn compute_node(&self, level: usize, index: usize) -> [u8; 32] {
        if level == 0 {
            return match &self.leaves[index] {
                Some((rid, sh)) => leaf_hash(rid, sh),
                None => empty_leaf_hash(),
            };
        }

        let left_idx = index * 2;
        let right_idx = index * 2 + 1;
        let child_level = level - 1;
        let max_at_child = 1usize << (SMT_DEPTH - child_level);

        let left = if left_idx < max_at_child {
            self.compute_node(child_level, left_idx)
        } else {
            Self::empty_subtree_hash(child_level)
        };

        let right = if right_idx < max_at_child {
            self.compute_node(child_level, right_idx)
        } else {
            Self::empty_subtree_hash(child_level)
        };

        branch_hash(&left, &right)
    }

    /// Generate a proof for the given resource.
    pub fn prove(&self, resource_id_hash: &[u8; 32]) -> SmtProof {
        let key = key_to_index(resource_id_hash) as usize;
        let mut siblings = [[0u8; 32]; SMT_DEPTH];

        let mut current_idx = key;
        for (level, sibling) in siblings.iter_mut().enumerate() {
            let sibling_idx = current_idx ^ 1;
            *sibling = self.compute_node(level, sibling_idx);
            current_idx /= 2;
        }

        SmtProof { siblings }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_rid(seed: u8) -> [u8; 32] {
        *blake3::hash(&[seed]).as_bytes()
    }

    fn make_state(seed: u8) -> [u8; 32] {
        *blake3::hash(&[seed, 0xFF]).as_bytes()
    }

    #[test]
    fn empty_leaf_deterministic() {
        assert_eq!(empty_leaf_hash(), empty_leaf_hash());
    }

    #[test]
    fn leaf_hash_deterministic() {
        let rid = make_rid(1);
        let sh = make_state(1);
        assert_eq!(leaf_hash(&rid, &sh), leaf_hash(&rid, &sh));
    }

    #[test]
    fn different_leaves_different_hashes() {
        let rid1 = make_rid(1);
        let rid2 = make_rid(2);
        let sh = make_state(1);
        assert_ne!(leaf_hash(&rid1, &sh), leaf_hash(&rid2, &sh));
    }

    #[cfg(feature = "std")]
    mod smt_tests {
        use super::*;

        #[test]
        fn empty_tree_deterministic() {
            let smt1 = Smt::new();
            let smt2 = Smt::new();
            assert_eq!(smt1.root(), smt2.root());
        }

        #[test]
        fn insert_changes_root() {
            let mut smt = Smt::new();
            let empty_root = smt.root();
            smt.upsert(make_rid(1), make_state(1));
            assert_ne!(smt.root(), empty_root);
        }

        #[test]
        fn proof_verification() {
            let mut smt = Smt::new();
            let rid = make_rid(42);
            let sh = make_state(42);
            smt.upsert(rid, sh);

            let root = smt.root();
            let proof = smt.prove(&rid);
            let lh = leaf_hash(&rid, &sh);
            let key = key_to_index(&rid);
            assert!(proof.verify(&root, key, &lh));

            // Wrong state hash should fail.
            let wrong = leaf_hash(&rid, &make_state(99));
            assert!(!proof.verify(&root, key, &wrong));
        }

        #[test]
        fn multiple_inserts_proof() {
            let mut smt = Smt::new();
            let entries: [(_, _); 3] = [
                (make_rid(10), make_state(10)),
                (make_rid(20), make_state(20)),
                (make_rid(30), make_state(30)),
            ];
            for (rid, sh) in &entries {
                smt.upsert(*rid, *sh);
            }

            let root = smt.root();
            for (rid, sh) in &entries {
                let proof = smt.prove(rid);
                let lh = leaf_hash(rid, sh);
                let key = key_to_index(rid);
                assert!(proof.verify(&root, key, &lh));
            }
        }

        #[test]
        fn update_changes_root() {
            let mut smt = Smt::new();
            let rid = make_rid(1);
            smt.upsert(rid, make_state(1));
            let root1 = smt.root();
            smt.upsert(rid, make_state(2));
            let root2 = smt.root();
            assert_ne!(root1, root2);
        }

        #[test]
        fn empty_proof_for_missing_key() {
            let mut smt = Smt::new();
            let rid1 = make_rid(10);
            smt.upsert(rid1, make_state(10));

            let rid2 = make_rid(20);
            let root = smt.root();
            let proof = smt.prove(&rid2);
            let empty = empty_leaf_hash();
            let key = key_to_index(&rid2);
            assert!(proof.verify(&root, key, &empty));
        }
    }
}
