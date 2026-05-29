use alloc::vec::Vec;

use tap::Tap;
use vprogs_core_codec::{Bits, Error, Reader, Result, Writer};
use vprogs_core_hashing::Hasher;

use crate::{
    EMPTY_HASH, Node,
    proving::{Leaf, Member, Membership, Traversal},
};

/// Zero-copy view of a multi-proof.
pub struct Proof<'a> {
    /// Deduplicated keys table. Caller-input keys come first; foreign witness keys follow.
    pub keys: &'a [[u8; 32]],
    /// Witness leaves.
    pub leaves: &'a [Leaf],
    /// Off-path sibling hashes for the proof tree.
    pub siblings: &'a [[u8; 32]],
    /// Bit-packed proof-tree topology.
    pub topology: &'a [u8],
    /// One [`Membership`] per caller-input key, in input order.
    pub memberships: &'a [Membership],
}

impl<'a> Proof<'a> {
    /// Decodes a multi-proof.
    pub fn decode(mut buf: &'a [u8]) -> Result<Self> {
        Ok(Self {
            keys: buf.slice_as::<[u8; 32]>("keys")?,
            leaves: buf.slice_as::<Leaf>("leaves")?,
            siblings: buf.slice_as::<[u8; 32]>("siblings")?,
            topology: buf.slice_as::<u8>("topology")?,
            memberships: buf.slice_as::<Membership>("memberships")?,
        })
    }

    /// Returns the [`Member`] at caller-input position `idx`, or an error if it doesn't exist.
    pub fn member(&self, idx: usize) -> Result<Member<'a>> {
        match *self.memberships.get(idx).ok_or(Error::Decode("membership out of range"))? {
            Membership::Present(leaf_idx) => self.member_present(idx, leaf_idx.get() as usize),
            Membership::Absent(leaf_idx) => self.member_absent(idx, leaf_idx.get() as usize),
        }
    }

    /// Iterates the proof's [`Member`]s in caller-input order.
    pub fn members(&self) -> impl Iterator<Item = Result<Member<'a>>> + '_ {
        (0..self.memberships.len()).map(|i| self.member(i))
    }

    /// Pre-state root from the proof's own leaf value hashes.
    pub fn root<H: Hasher>(&self) -> Result<[u8; 32]> {
        Traversal::compute_root::<H>(self, |i| self.leaf_hash::<H>(i))
    }

    /// Post-state root with each member's new value supplied by `value(member_idx)`.
    pub fn new_root<H: Hasher>(&self, value: impl Fn(usize) -> &'a [u8; 32]) -> Result<[u8; 32]> {
        let mut buf = Vec::new();
        Traversal::compute_root::<H>(self, |i| self.new_subtree_hash::<H>(i, &value, &mut buf))
    }

    /// Encodes the proof into the wire format.
    pub(crate) fn encode(&self) -> Vec<u8> {
        Vec::with_capacity(self.wire_size()).tap_mut(|buf| {
            buf.write_slice(self.keys);
            buf.write_slice(self.leaves);
            buf.write_slice(self.siblings);
            buf.write_slice(self.topology);
            buf.write_slice(self.memberships);
        })
    }

    /// Resolves a `Present` claim into a [`Member`], or errs if the leaf doesn't own the key.
    fn member_present(&self, idx: usize, leaf_idx: usize) -> Result<Member<'a>> {
        // Resolve the queried key and the wire-claimed witness leaf.
        let key = self.key(idx)?;
        let leaf = self.leaf(leaf_idx)?;

        // The leaf must be the queried key's own slot.
        if self.leaf_key(leaf)? != key {
            return Err(Error::Decode("leaf is absent"));
        }

        Ok(Member { key, leaf, absent: false })
    }

    /// Resolves an `Absent` claim into a [`Member`].
    fn member_absent(&self, idx: usize, leaf_idx: usize) -> Result<Member<'a>> {
        // Resolve the queried key, the wire-claimed witness leaf, and the leaf's own key.
        let key = self.key(idx)?;
        let leaf = self.leaf(leaf_idx)?;
        let leaf_key = self.leaf_key(leaf)?;

        // The leaf must NOT be the queried key's own slot - it must be a foreign witness.
        if leaf_key == key {
            return Err(Error::Decode("leaf is present"));
        }

        // The foreign witness must share the queried key's path at its declared depth.
        if !key.shares_prefix(leaf_key, leaf.depth.get() as usize) {
            return Err(Error::Decode("absent leaf does not witness key"));
        }

        Ok(Member { key, leaf, absent: true })
    }

    /// Pre-state hash at proof-leaf position `i`: the existing leaf's `hash_leaf(key, value_hash)`.
    fn leaf_hash<H: Hasher>(&self, i: usize) -> Result<[u8; 32]> {
        let leaf = self.leaf(i)?;
        Ok(Node::hash_leaf::<H>(self.leaf_key(leaf)?, &leaf.value_hash))
    }

    /// Post-state hash at proof-leaf position `leaf_pos`, synthesized from members landing there.
    fn new_subtree_hash<H: Hasher>(
        &self,
        leaf_pos: usize,
        new_value: impl Fn(usize) -> &'a [u8; 32],
        buffer: &mut Vec<(&'a [u8; 32], &'a [u8; 32])>,
    ) -> Result<[u8; 32]> {
        buffer.clear();
        let witness_covered = self.members_at_leaf(leaf_pos, &new_value, buffer)?;

        // Include the witness key if no Present member covers it, and it still has a value.
        let leaf = self.leaf(leaf_pos)?;
        if !witness_covered && leaf.value_hash != EMPTY_HASH {
            buffer.push((self.leaf_key(leaf)?, &leaf.value_hash));
        }

        buffer.sort_by(|a, b| a.0.cmp(b.0));
        Ok(Self::subtree_hash::<H>(buffer.as_slice(), leaf.depth.get()))
    }

    /// Gathers members at `leaf_pos` into `buffer`; returns whether a `Present` member covers it.
    fn members_at_leaf(
        &self,
        leaf_pos: usize,
        new_value: impl Fn(usize) -> &'a [u8; 32],
        buffer: &mut Vec<(&'a [u8; 32], &'a [u8; 32])>,
    ) -> Result<bool> {
        let mut witness_covered = false;
        for (i, m) in self.memberships.iter().enumerate() {
            let (is_present, m_leaf_pos) = match *m {
                Membership::Present(p) => (true, p.get() as usize),
                Membership::Absent(p) => (false, p.get() as usize),
            };

            if m_leaf_pos == leaf_pos {
                if is_present {
                    witness_covered = true;
                }
                let v = new_value(i);
                if v != &EMPTY_HASH {
                    buffer.push((self.key(i)?, v));
                }
            }
        }
        Ok(witness_covered)
    }

    /// Hashes the subtree rooted at `depth` containing `entries` sorted by key sharing a prefix.
    fn subtree_hash<H: Hasher>(entries: &[(&[u8; 32], &[u8; 32])], depth: u16) -> [u8; 32] {
        // Empty subtree - the universal EMPTY_HASH.
        if entries.is_empty() {
            return EMPTY_HASH;
        }

        // Single entry - emit as a shortcut leaf at this depth.
        if entries.len() == 1 {
            return Node::hash_leaf::<H>(entries[0].0, entries[0].1);
        }

        // Multiple entries - split by the bit at `depth` and combine the halves.
        let mid = entries.partition_point(|(k, _)| !k.get_msb(depth as usize));
        let (left, right) = entries.split_at(mid);

        Node::hash_internal::<H>(
            &Self::subtree_hash::<H>(left, depth + 1),
            &Self::subtree_hash::<H>(right, depth + 1),
        )
    }

    /// Byte length the proof occupies in the wire format: five length-prefixed sections.
    fn wire_size(&self) -> usize {
        20 + size_of_val(self.keys)
            + size_of_val(self.leaves)
            + size_of_val(self.siblings)
            + size_of_val(self.topology)
            + size_of_val(self.memberships)
    }

    /// Bounds-checked access to `keys[idx]`.
    fn key(&self, idx: usize) -> Result<&'a [u8; 32]> {
        self.keys.get(idx).ok_or(Error::Decode("key out of range"))
    }

    /// Bounds-checked access to `leaves[idx]`.
    fn leaf(&self, idx: usize) -> Result<&'a Leaf> {
        self.leaves.get(idx).ok_or(Error::Decode("leaf out of range"))
    }

    /// Bounds-checked access to the leaf's own key (i.e. `keys[leaf.key_idx]`).
    fn leaf_key(&self, leaf: &Leaf) -> Result<&'a [u8; 32]> {
        self.key(leaf.key_idx.get() as usize)
    }
}
