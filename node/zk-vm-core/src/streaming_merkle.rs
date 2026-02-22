//! Generic streaming Merkle tree builder — no heap allocation.
//!
//! Uses a fixed-size stack to build the tree incrementally. Parameterized
//! by a [`MerkleHashOps`] trait that supplies the branch hash and empty
//! subtree hash functions, so the same builder works for effects trees,
//! seq-commitment trees, and any other domain-specific tree.

/// Maximum stack depth — supports up to 2^32 leaves.
const MAX_STACK: usize = 32;

/// Operations that define a particular Merkle tree domain.
pub trait MerkleHashOps {
    /// Hash two children into a parent.
    fn branch(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32];

    /// Empty subtree hash at the given level.
    ///
    /// Level 0 = the empty *leaf* sentinel.
    /// Level n = `branch(empty(n-1), empty(n-1))`.
    fn empty_subtree(level: usize) -> [u8; 32];
}

/// Streaming (stack-based) Merkle tree builder.
///
/// Processes leaves one at a time with O(1) amortised work per leaf and
/// O(log N) stack space — all on the program stack (no heap).
pub struct StreamingMerkle<H: MerkleHashOps> {
    stack: [(u32, [u8; 32]); MAX_STACK],
    stack_len: usize,
    leaf_count: u32,
    _h: core::marker::PhantomData<H>,
}

impl<H: MerkleHashOps> Default for StreamingMerkle<H> {
    fn default() -> Self {
        Self::new()
    }
}

impl<H: MerkleHashOps> StreamingMerkle<H> {
    pub fn new() -> Self {
        Self {
            stack: [(0, [0u8; 32]); MAX_STACK],
            stack_len: 0,
            leaf_count: 0,
            _h: core::marker::PhantomData,
        }
    }

    /// Add a pre-hashed leaf.
    pub fn add_leaf(&mut self, hash: [u8; 32]) {
        let mut level = 0u32;
        let mut current = hash;

        while self.stack_len > 0 {
            let (top_level, top_hash) = self.stack[self.stack_len - 1];
            if top_level != level {
                break;
            }
            self.stack_len -= 1;
            current = H::branch(&top_hash, &current);
            level += 1;
        }

        self.stack[self.stack_len] = (level, current);
        self.stack_len += 1;
        self.leaf_count += 1;
    }

    /// Number of leaves added so far.
    pub fn leaf_count(&self) -> u32 {
        self.leaf_count
    }

    /// Finalize and return the Merkle root.
    ///
    /// Pads incomplete subtrees with the domain-specific empty subtree hashes.
    pub fn finalize(self) -> [u8; 32] {
        if self.leaf_count == 0 {
            return H::empty_subtree(0);
        }

        if self.leaf_count == 1 {
            return H::branch(&self.stack[0].1, &H::empty_subtree(0));
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
                result_hash = H::branch(&result_hash, &H::empty_subtree(result_level as usize));
                result_level += 1;
            }

            result_hash = H::branch(&hash, &result_hash);
            result_level += 1;
        }

        result_hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashing::domain_to_key;

    /// Simple test hash ops using blake3 with zero-padding for empty subtrees.
    struct TestHashOps;

    impl MerkleHashOps for TestHashOps {
        fn branch(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
            const KEY: [u8; blake3::KEY_LEN] = domain_to_key(b"TestMerkleBranch");
            let mut hasher = blake3::Hasher::new_keyed(&KEY);
            hasher.update(left);
            hasher.update(right);
            *hasher.finalize().as_bytes()
        }

        fn empty_subtree(_level: usize) -> [u8; 32] {
            [0u8; 32]
        }
    }

    fn make_leaf(i: u8) -> [u8; 32] {
        *blake3::hash(&[i]).as_bytes()
    }

    #[test]
    fn empty_tree() {
        let builder = StreamingMerkle::<TestHashOps>::new();
        let root = builder.finalize();
        assert_eq!(root, [0u8; 32]);
    }

    #[test]
    fn single_leaf() {
        let leaf = make_leaf(1);
        let mut builder = StreamingMerkle::<TestHashOps>::new();
        builder.add_leaf(leaf);
        let root = builder.finalize();
        assert_eq!(root, TestHashOps::branch(&leaf, &[0u8; 32]));
    }

    #[test]
    fn two_leaves() {
        let l1 = make_leaf(1);
        let l2 = make_leaf(2);

        let mut builder = StreamingMerkle::<TestHashOps>::new();
        builder.add_leaf(l1);
        builder.add_leaf(l2);
        let root = builder.finalize();

        assert_eq!(root, TestHashOps::branch(&l1, &l2));
    }

    #[test]
    fn three_leaves() {
        let l1 = make_leaf(1);
        let l2 = make_leaf(2);
        let l3 = make_leaf(3);

        let mut builder = StreamingMerkle::<TestHashOps>::new();
        builder.add_leaf(l1);
        builder.add_leaf(l2);
        builder.add_leaf(l3);
        let root = builder.finalize();

        let left = TestHashOps::branch(&l1, &l2);
        let right = TestHashOps::branch(&l3, &[0u8; 32]);
        assert_eq!(root, TestHashOps::branch(&left, &right));
    }

    #[test]
    fn four_leaves() {
        let leaves: [_; 4] = core::array::from_fn(|i| make_leaf(i as u8));

        let mut builder = StreamingMerkle::<TestHashOps>::new();
        for leaf in &leaves {
            builder.add_leaf(*leaf);
        }
        let root = builder.finalize();

        let left = TestHashOps::branch(&leaves[0], &leaves[1]);
        let right = TestHashOps::branch(&leaves[2], &leaves[3]);
        assert_eq!(root, TestHashOps::branch(&left, &right));
    }

    #[test]
    fn deterministic() {
        let leaves: [_; 5] = core::array::from_fn(|i| make_leaf(i as u8));

        let build = || {
            let mut b = StreamingMerkle::<TestHashOps>::new();
            for leaf in &leaves {
                b.add_leaf(*leaf);
            }
            b.finalize()
        };

        assert_eq!(build(), build());
    }

    #[test]
    fn order_matters() {
        let l1 = make_leaf(1);
        let l2 = make_leaf(2);

        let mut b1 = StreamingMerkle::<TestHashOps>::new();
        b1.add_leaf(l1);
        b1.add_leaf(l2);

        let mut b2 = StreamingMerkle::<TestHashOps>::new();
        b2.add_leaf(l2);
        b2.add_leaf(l1);

        assert_ne!(b1.finalize(), b2.finalize());
    }
}
