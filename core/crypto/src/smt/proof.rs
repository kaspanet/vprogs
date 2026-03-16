use alloc::vec::Vec;

use super::state_commitment::StateCommitment;
use crate::{
    EMPTY_HASH, Hasher,
    utils::{bools::Bools, bytes::Bits},
};

/// Size of a single leaf entry in the wire format: depth(2) + key(32) + value_hash(32).
pub(crate) const LEAF_ENTRY_SIZE: usize = 66;

/// Zero-copy view of a multi-proof, borrowing from a flat byte buffer.
///
/// Wire format:
/// - `n_leaves: u32` + `[depth(u16) + key(32) + value_hash(32)] x n_leaves`
/// - `n_siblings: u32` + `[u8; 32] x n_siblings`
/// - `topology_len: u32` + `topology_bytes`
///
/// During verification, traversal stops at each leaf's declared depth and computes `hash_leaf(key,
/// value_hash)` instead of recursing all the way to depth 256. Shortcut leaves at shallow depths
/// mean far fewer hashes.
pub struct Proof<'a> {
    buf: &'a [u8],
    n_leaves: u32,
    siblings_offset: usize,
    topology_offset: usize,
    topology_len: usize,
}

impl<'a> Proof<'a> {
    /// Decodes a multi-proof from a flat byte buffer.
    pub fn decode(buf: &'a [u8]) -> Self {
        // Section 1: leaf entries.
        let n_leaves = u32::from_le_bytes(buf[0..4].try_into().expect("truncated n_leaves"));
        let leaves_end = 4 + (n_leaves as usize) * LEAF_ENTRY_SIZE;

        // Section 2: sibling hashes.
        let n_siblings = u32::from_le_bytes(
            buf[leaves_end..leaves_end + 4].try_into().expect("truncated n_siblings"),
        );
        let siblings_offset = leaves_end + 4;
        let siblings_end = siblings_offset + (n_siblings as usize) * 32;

        // Section 3: topology bitfield.
        let topology_len = u32::from_le_bytes(
            buf[siblings_end..siblings_end + 4].try_into().expect("truncated topology_len"),
        ) as usize;
        let topology_offset = siblings_end + 4;

        Self { buf, n_leaves, siblings_offset, topology_offset, topology_len }
    }

    /// Encodes proof components into the wire format.
    pub(crate) fn encode(
        leaves: &[(u16, StateCommitment)],
        siblings: &[[u8; 32]],
        topology_bits: &[bool],
    ) -> Vec<u8> {
        let topology = topology_bits.pack_lsb();
        let total =
            4 + leaves.len() * LEAF_ENTRY_SIZE + 4 + siblings.len() * 32 + 4 + topology.len();
        let mut buf = Vec::with_capacity(total);

        // Section 1: leaf entries — each is depth(2) + key(32) + value_hash(32) = 66 bytes.
        buf.extend_from_slice(&(leaves.len() as u32).to_le_bytes());
        for &(depth, StateCommitment { key, value_hash }) in leaves {
            buf.extend_from_slice(&depth.to_le_bytes());
            buf.extend_from_slice(&key);
            buf.extend_from_slice(&value_hash);
        }

        // Section 2: sibling hashes — each is 32 bytes.
        buf.extend_from_slice(&(siblings.len() as u32).to_le_bytes());
        for sibling in siblings {
            buf.extend_from_slice(sibling);
        }

        // Section 3: topology bitfield — variable length.
        buf.extend_from_slice(&(topology.len() as u32).to_le_bytes());
        buf.extend_from_slice(&topology);

        debug_assert_eq!(buf.len(), total);
        buf
    }

    /// Returns the number of leaves in the proof.
    pub fn n_leaves(&self) -> usize {
        self.n_leaves as usize
    }

    /// Returns the depth of the leaf at index `i`.
    pub fn leaf_depth(&self, i: usize) -> u16 {
        u16::from_le_bytes(
            self.buf[4 + i * LEAF_ENTRY_SIZE..][..2].try_into().expect("truncated depth"),
        )
    }

    /// Returns the key of the leaf at index `i`.
    pub fn leaf_key(&self, i: usize) -> &[u8; 32] {
        self.buf[4 + i * LEAF_ENTRY_SIZE + 2..][..32].try_into().expect("truncated key")
    }

    /// Returns the value hash of the leaf at index `i`.
    pub fn leaf_value_hash(&self, i: usize) -> &[u8; 32] {
        self.buf[4 + i * LEAF_ENTRY_SIZE + 34..][..32].try_into().expect("truncated value_hash")
    }

    /// Verifies that the proof leaves produce the expected root hash.
    pub fn verify<H: Hasher>(&self, expected_root: [u8; 32]) -> bool {
        self.compute_root_with::<H>(|i| self.leaf_value_hash(i)) == expected_root
    }

    /// Recomputes the root using updated value hashes.
    ///
    /// `updated_hashes` must have the same length as `n_leaves()` and provides the new value hash
    /// for each leaf (in the same order as in the proof). This enables computing the post-update
    /// root without re-reading the tree.
    pub fn compute_root<H: Hasher>(&self, updated_hashes: &[[u8; 32]]) -> [u8; 32] {
        assert_eq!(updated_hashes.len(), self.n_leaves());
        self.compute_root_with::<H>(|i| &updated_hashes[i])
    }

    /// Returns the sibling hash at index `i`.
    fn sibling(&self, i: usize) -> &[u8; 32] {
        self.buf[self.siblings_offset + i * 32..][..32].try_into().expect("truncated sibling")
    }

    /// Returns the topology bytes.
    fn topology(&self) -> &[u8] {
        &self.buf[self.topology_offset..][..self.topology_len]
    }

    /// Shared traversal logic parameterized on which value hash to use for each leaf.
    fn compute_root_with<'b, H: Hasher>(
        &self,
        value_hash_fn: impl Fn(usize) -> &'b [u8; 32],
    ) -> [u8; 32] {
        if self.n_leaves() == 0 {
            return EMPTY_HASH;
        }

        // Start traversal from the root (bit_pos 0) with all leaf indices.
        let mut sibling_idx = 0;
        let mut topo_bit = 0usize;
        self.traverse::<H>(0, self.n_leaves(), 0, &value_hash_fn, &mut sibling_idx, &mut topo_bit)
    }

    /// Recursive traversal of the shortcut-aware proof tree.
    ///
    /// `start..end` is the range of leaf indices that fall within this subtree. Because proof
    /// leaves are sorted by key and MSB-first bit ordering matches lexicographic order, splitting
    /// by any bit always produces two contiguous sub-ranges — so ranges suffice (no Vec allocations
    /// needed).
    fn traverse<'b, H: Hasher>(
        &self,
        start: usize,
        end: usize,
        bit_pos: usize,
        value_hash_fn: &impl Fn(usize) -> &'b [u8; 32],
        sibling_idx: &mut usize,
        topo_bit: &mut usize,
    ) -> [u8; 32] {
        if start == end {
            return EMPTY_HASH;
        }

        // Shortcut leaf check: if there's exactly one leaf and we've reached its declared depth,
        // compute the leaf hash directly instead of recursing further.
        if end - start == 1 {
            let depth = self.leaf_depth(start) as usize;
            if bit_pos == depth {
                let key = self.leaf_key(start);
                return H::hash_leaf(key, value_hash_fn(start));
            }
        }

        // Read the next topology bit to determine the structure at this level.
        let bit_val = self.topology().get_lsb(*topo_bit);
        *topo_bit += 1;

        if bit_val {
            // Topology bit = 1: both children have proof leaves — find the split point and
            // recurse both sides.
            let mid = self.split_point(start, end, bit_pos);
            let left =
                self.traverse::<H>(start, mid, bit_pos + 1, value_hash_fn, sibling_idx, topo_bit);
            let right =
                self.traverse::<H>(mid, end, bit_pos + 1, value_hash_fn, sibling_idx, topo_bit);
            H::hash_internal(&left, &right)
        } else {
            // Topology bit = 0: only one side has proof leaves — use a sibling hash for the other.
            let goes_left = !self.leaf_key(start).get_msb(bit_pos);
            let sibling = *self.sibling(*sibling_idx);
            *sibling_idx += 1;

            let child =
                self.traverse::<H>(start, end, bit_pos + 1, value_hash_fn, sibling_idx, topo_bit);
            if goes_left {
                H::hash_internal(&child, &sibling)
            } else {
                H::hash_internal(&sibling, &child)
            }
        }
    }

    /// Finds the partition point where keys switch from bit=0 (left) to bit=1 (right).
    fn split_point(&self, start: usize, end: usize, bit_pos: usize) -> usize {
        let mut mid = start;
        while mid < end && !self.leaf_key(mid).get_msb(bit_pos) {
            mid += 1;
        }
        mid
    }
}
