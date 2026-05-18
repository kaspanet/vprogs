use core::marker::PhantomData;

use vprogs_core_hashing::Hasher;

use crate::NodeTags;

/// Streaming dense Merkle tree builder - stack of `(level, hash)` pairs, no heap allocation.
///
/// Append leaves with [`add_leaf`] (or [`add_leaf_parts`] for multi-part payloads) and call
/// [`finalize`] to extract the root at the builder's natural depth
/// ([`required_depth`]`(leaf_count)` levels). The rightmost incomplete subtree is padded with
/// the appropriate empty-subtree hashes (supplied to `finalize` as a precomputed table) when
/// assembling the root.
///
/// The builder holds at most one stack entry per distinct tree level (i.e. `popcount(leaf_count)`
/// entries), which peaks at `MAX_DEPTH` for a `u32` leaf count - the stack is sized to that
/// bound.
///
/// [`add_leaf`]: StreamingBuilder::add_leaf
/// [`add_leaf_parts`]: StreamingBuilder::add_leaf_parts
/// [`finalize`]: StreamingBuilder::finalize
/// [`required_depth`]: StreamingBuilder::required_depth
pub struct StreamingBuilder<H, T, const MAX_DEPTH: usize, const TAG_N: usize>
where
    H: Hasher,
    T: NodeTags<TAG_N>,
{
    stack: [(u32, [u8; 32]); MAX_DEPTH],
    stack_len: usize,
    leaf_count: u32,
    _marker: PhantomData<(H, T)>,
}

impl<H, T, const MAX_DEPTH: usize, const TAG_N: usize> Default
    for StreamingBuilder<H, T, MAX_DEPTH, TAG_N>
where
    H: Hasher,
    T: NodeTags<TAG_N>,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<H, T, const MAX_DEPTH: usize, const TAG_N: usize> StreamingBuilder<H, T, MAX_DEPTH, TAG_N>
where
    H: Hasher,
    T: NodeTags<TAG_N>,
{
    /// The maximum tree depth this builder is sized for (the `MAX_DEPTH` const generic).
    pub const MAX_DEPTH: usize = MAX_DEPTH;

    /// The byte length of this builder's [`NodeTags`] (the `TAG_N` const generic).
    pub const TAG_N: usize = TAG_N;

    /// Creates an empty builder.
    pub fn new() -> Self {
        Self {
            stack: [(0, [0u8; 32]); MAX_DEPTH],
            stack_len: 0,
            leaf_count: 0,
            _marker: PhantomData,
        }
    }

    /// Required tree depth for `count` leaves: `ceil(log2(count))`, clamped to `[1, MAX_DEPTH]`.
    ///
    /// Returns `1` for 0 or 1 leaves (an empty / single-leaf tree still has a root at depth 1).
    pub fn required_depth(count: usize) -> usize {
        if count <= 1 {
            return 1;
        }
        let bits = usize::BITS - (count - 1).leading_zeros();
        (bits as usize).min(MAX_DEPTH)
    }

    /// Leaf-hash of a single payload, domain-separated by [`NodeTags::LEAF`].
    pub fn hash_leaf(payload: impl AsRef<[u8]>) -> [u8; 32] {
        H::hash_with_domain(T::LEAF, payload)
    }

    /// Leaf-hash of the concatenation of payload parts, domain-separated by [`NodeTags::LEAF`].
    pub fn hash_leaf_parts(parts: impl IntoIterator<Item = impl AsRef<[u8]>>) -> [u8; 32] {
        H::hash_parts_with_domain(T::LEAF, parts)
    }

    /// Branch-hash of two child hashes, domain-separated by [`NodeTags::BRANCH`].
    pub fn hash_branch(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
        H::hash_parts_with_domain(T::BRANCH, [left, right])
    }

    /// Hash of the empty leaf marker, domain-separated by [`NodeTags::EMPTY`].
    pub fn hash_empty() -> [u8; 32] {
        H::hash_with_domain(T::EMPTY, b"")
    }

    /// Computes the empty-subtree hash table for this builder's configuration.
    ///
    /// - `table[0]` is [`hash_empty`].
    /// - `table[L]` for `L > 0` is [`hash_branch`]`(table[L-1], table[L-1])`, the root of an
    ///   all-empty subtree of height `L`.
    ///
    /// Intended for use from a build script: compute the table once at build time, emit it as a
    /// `const`, then `include!` the generated file at runtime. Avoids recomputing the table
    /// inside the guest on every batch.
    ///
    /// [`hash_empty`]: StreamingBuilder::hash_empty
    /// [`hash_branch`]: StreamingBuilder::hash_branch
    pub fn compute_empty_hashes() -> [[u8; 32]; MAX_DEPTH] {
        let mut table = [[0u8; 32]; MAX_DEPTH];
        if MAX_DEPTH == 0 {
            return table;
        }
        table[0] = Self::hash_empty();
        for level in 1..MAX_DEPTH {
            let prev = table[level - 1];
            table[level] = Self::hash_branch(&prev, &prev);
        }
        table
    }

    /// Appends a leaf by hashing its single payload with [`NodeTags::LEAF`].
    pub fn add_leaf(&mut self, payload: impl AsRef<[u8]>) {
        self.add_leaf_hash(Self::hash_leaf(payload));
    }

    /// Appends a leaf by hashing the concatenation of payload parts with [`NodeTags::LEAF`].
    pub fn add_leaf_parts(&mut self, parts: impl IntoIterator<Item = impl AsRef<[u8]>>) {
        self.add_leaf_hash(Self::hash_leaf_parts(parts));
    }

    /// Returns the number of leaves added so far.
    pub fn leaf_count(&self) -> u32 {
        self.leaf_count
    }

    /// Finalizes the tree, padding incomplete subtrees with the empty-subtree hash table.
    pub fn finalize(&self, empty_hashes: &[[u8; 32]; MAX_DEPTH]) -> [u8; 32] {
        if self.leaf_count == 0 {
            return empty_hashes[0];
        }

        if self.leaf_count == 1 {
            return Self::hash_branch(&self.stack[0].1, &empty_hashes[0]);
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
                result_hash = Self::hash_branch(&result_hash, &empty_hashes[result_level as usize]);
                result_level += 1;
            }

            result_hash = Self::hash_branch(&hash, &result_hash);
            result_level += 1;
        }

        result_hash
    }

    /// Adds a pre-computed leaf hash to the tree, combining it with same-level stack entries
    /// on the right edge.
    fn add_leaf_hash(&mut self, hash: [u8; 32]) {
        let mut level = 0u32;
        let mut current = hash;

        while self.stack_len > 0 {
            let (top_level, top_hash) = self.stack[self.stack_len - 1];
            if top_level != level {
                break;
            }
            self.stack_len -= 1;
            current = Self::hash_branch(&top_hash, &current);
            level += 1;
        }

        self.stack[self.stack_len] = (level, current);
        self.stack_len += 1;
        self.leaf_count += 1;
    }
}

#[cfg(test)]
mod tests {
    use vprogs_core_hashing::Blake3;

    use super::*;

    struct TestTags;

    impl NodeTags<1> for TestTags {
        const LEAF: &'static [u8; 1] = &[0x00];
        const BRANCH: &'static [u8; 1] = &[0x01];
        const EMPTY: &'static [u8; 1] = &[0x02];
    }

    type TestBuilder = StreamingBuilder<Blake3, TestTags, 8, 1>;

    #[test]
    fn required_depth_clamps() {
        assert_eq!(TestBuilder::required_depth(0), 1);
        assert_eq!(TestBuilder::required_depth(1), 1);
        assert_eq!(TestBuilder::required_depth(2), 1);
        assert_eq!(TestBuilder::required_depth(3), 2);
        assert_eq!(TestBuilder::required_depth(4), 2);
        assert_eq!(TestBuilder::required_depth(5), 3);
        assert_eq!(TestBuilder::required_depth(256), TestBuilder::MAX_DEPTH);
        assert_eq!(
            TestBuilder::required_depth(1 << TestBuilder::MAX_DEPTH),
            TestBuilder::MAX_DEPTH
        );
        assert_eq!(
            TestBuilder::required_depth((1 << TestBuilder::MAX_DEPTH) + 1),
            TestBuilder::MAX_DEPTH
        );
        assert_eq!(TestBuilder::required_depth(usize::MAX), TestBuilder::MAX_DEPTH);
    }

    #[test]
    fn finalize_three_leaves_matches_manual_padded_root() {
        // Exercises the multi-entry combine and the `while result_level < level` padding loop
        // that the power-of-2 counts (1, 2) never reach.
        let payloads: [&[u8]; 3] = [b"leaf-a", b"leaf-b", b"leaf-c"];
        let empty_hashes = TestBuilder::compute_empty_hashes();
        let mut builder = TestBuilder::new();
        for p in payloads {
            builder.add_leaf(p);
        }
        assert_eq!(TestBuilder::required_depth(3), 2);

        // depth-2 padded tree over leaves [h0, h1, h2, empty].
        let h: [[u8; 32]; 3] = payloads.map(TestBuilder::hash_leaf);
        let b01 = TestBuilder::hash_branch(&h[0], &h[1]);
        let b2e = TestBuilder::hash_branch(&h[2], &empty_hashes[0]);
        let root = TestBuilder::hash_branch(&b01, &b2e);
        assert_eq!(builder.finalize(&empty_hashes), root);
    }

    #[test]
    fn finalize_five_leaves_matches_manual_padded_root() {
        // The right subtree pads two levels deep, so the padding loop runs more than once.
        let payloads: [&[u8]; 5] = [b"a", b"b", b"c", b"d", b"e"];
        let empty_hashes = TestBuilder::compute_empty_hashes();
        let mut builder = TestBuilder::new();
        for p in payloads {
            builder.add_leaf(p);
        }
        assert_eq!(TestBuilder::required_depth(5), 3);

        // depth-3 padded tree over leaves [h0, h1, h2, h3, h4, empty, empty, empty].
        let h: [[u8; 32]; 5] = payloads.map(TestBuilder::hash_leaf);
        let left = TestBuilder::hash_branch(
            &TestBuilder::hash_branch(&h[0], &h[1]),
            &TestBuilder::hash_branch(&h[2], &h[3]),
        );
        let right = TestBuilder::hash_branch(
            &TestBuilder::hash_branch(&h[4], &empty_hashes[0]),
            &empty_hashes[1],
        );
        let root = TestBuilder::hash_branch(&left, &right);
        assert_eq!(builder.finalize(&empty_hashes), root);
    }

    #[test]
    fn compute_empty_hashes_recurrence() {
        // Level 0 is `hash_empty`; each level up is `hash_branch` of the level below itself.
        let table = TestBuilder::compute_empty_hashes();
        let mut expected = TestBuilder::hash_empty();
        assert_eq!(table[0], expected, "level 0");
        for (level, &h) in table.iter().enumerate().skip(1) {
            expected = TestBuilder::hash_branch(&expected, &expected);
            assert_eq!(h, expected, "level {level}");
        }
    }
}
