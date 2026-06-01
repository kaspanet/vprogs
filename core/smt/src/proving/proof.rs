use alloc::{vec, vec::Vec};

use tap::Tap;
use vprogs_core_codec::{Bits, Error, Reader, Result, Writer};
use vprogs_core_hashing::Hasher;

use crate::{
    EMPTY_HASH, HashedNode,
    proving::{Leaf, LeafUpdate, Member, MembersByLeaf, Membership, Traversal},
};

/// Zero-copy view of a multi-proof.
pub struct Proof<'a> {
    /// Deduplicated keys table. Caller-input keys come first; foreign witness keys follow.
    pub keys: &'a [[u8; 32]],
    /// Witness leaves.
    pub leaves: &'a [Leaf],
    /// Off-path sibling summaries (kind + hash) for the proof tree.
    pub siblings: &'a [HashedNode],
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
            siblings: buf.slice_as::<HashedNode>("siblings")?,
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
        Traversal::new(self, |i| self.leaf_hash::<H>(i)).traverse::<H>(0, self.leaves.len(), 0)
    }

    /// Post-state root with each member's new value supplied by `value(member_idx)`.
    pub fn new_root<H: Hasher>(&self, value: impl Fn(usize) -> &'a [u8; 32]) -> Result<[u8; 32]> {
        Ok(self.compute_roots::<H>(value)?.1)
    }

    /// Both roots in one walk; unchanged subtrees reuse the pre-state hash.
    pub fn compute_roots<H: Hasher>(
        &self,
        new_value: impl Fn(usize) -> &'a [u8; 32],
    ) -> Result<([u8; 32], [u8; 32])> {
        let mbl = self.members_by_leaf();
        let mut buf = Vec::new();
        Traversal::new(self, |leaf_pos| {
            self.leaf_pair::<H, _>(leaf_pos, mbl.at(leaf_pos), &new_value, &mut buf)
        })
        .traverse_pair::<H>(0, self.leaves.len(), 0)
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

    /// Resolves an `Absent` claim into a [`Member`], or errs if the leaf doesn't witness absence.
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

    /// Pre-state at leaf position `i`: the existing leaf's `HashedNode::leaf(key, value_hash)`.
    #[inline]
    fn leaf_hash<H: Hasher>(&self, i: usize) -> Result<HashedNode> {
        let leaf = self.leaf(i)?;
        Ok(HashedNode::leaf::<H>(self.leaf_key(leaf)?, &leaf.value_hash))
    }

    /// Builds the CSR-flat inversion of memberships by proof-leaf position.
    fn members_by_leaf(&self) -> MembersByLeaf {
        let leaves_len = self.leaves.len();
        let mut offsets = vec![0u32; leaves_len + 1];

        // Pass 1: count members per leaf into `offsets[leaf_pos + 1]`.
        for m in self.memberships.iter() {
            let leaf_pos = match *m {
                Membership::Present(p) | Membership::Absent(p) => p.get() as usize,
            };
            // Out-of-range leaf_pos is malformed wire; surfaces downstream as a decode error.
            if leaf_pos < leaves_len {
                offsets[leaf_pos + 1] += 1;
            }
        }

        // Prefix sum: offsets[i] = start of leaf i's range in `indices`.
        for i in 1..=leaves_len {
            offsets[i] += offsets[i - 1];
        }

        // Pass 2: fill `indices`, using `offsets[leaf_pos]` as a write cursor; shift right after.
        let mut indices = vec![0u32; offsets[leaves_len] as usize];
        for (i, m) in self.memberships.iter().enumerate() {
            let leaf_pos = match *m {
                Membership::Present(p) | Membership::Absent(p) => p.get() as usize,
            };
            if leaf_pos < leaves_len {
                indices[offsets[leaf_pos] as usize] = i as u32;
                offsets[leaf_pos] += 1;
            }
        }
        for i in (1..=leaves_len).rev() {
            offsets[i] = offsets[i - 1];
        }
        offsets[0] = 0;

        MembersByLeaf { offsets, indices }
    }

    /// Pre and post-state summaries at a proof-leaf; reuses pre for post when nothing changes.
    fn leaf_pair<H: Hasher, F: Fn(usize) -> &'a [u8; 32]>(
        &self,
        leaf_pos: usize,
        member_indices: &[u32],
        new_value: &F,
        buffer: &mut Vec<(&'a [u8; 32], &'a [u8; 32])>,
    ) -> Result<(HashedNode, HashedNode)> {
        let leaf = self.leaf(leaf_pos)?;
        let leaf_key = self.leaf_key(leaf)?;
        let prev = HashedNode::leaf::<H>(leaf_key, &leaf.value_hash);

        let post =
            match self.classify_leaf_update(leaf, leaf_key, member_indices, new_value, buffer)? {
                LeafUpdate::Unchanged => prev,
                LeafUpdate::Single(value) => HashedNode::leaf::<H>(leaf_key, value),
                LeafUpdate::Multiple => {
                    Self::subtree_hash::<H>(buffer.as_slice(), leaf.depth.get())
                }
            };

        Ok((prev, post))
    }

    /// Classifies the post-state at a leaf, filling `buffer` with sorted entries for synthesis.
    fn classify_leaf_update<F: Fn(usize) -> &'a [u8; 32]>(
        &self,
        leaf: &'a Leaf,
        leaf_key: &'a [u8; 32],
        member_indices: &[u32],
        new_value: &F,
        buffer: &mut Vec<(&'a [u8; 32], &'a [u8; 32])>,
    ) -> Result<LeafUpdate<'a>> {
        // Single pass over members AT this leaf: detect change, gather entries for synthesis.
        buffer.clear();
        let mut new_witness_value: Option<&'a [u8; 32]> = None;
        let mut needs_synthesis = false;
        let mut any_change = false;
        for &i in member_indices {
            let m = self.memberships[i as usize];
            let v = new_value(i as usize);
            match m {
                Membership::Present(_) => {
                    new_witness_value = Some(v);
                    if v != &leaf.value_hash {
                        any_change = true;
                    }
                }
                Membership::Absent(_) => {
                    if v != &EMPTY_HASH {
                        needs_synthesis = true;
                        any_change = true;
                    }
                }
            }
            if v != &EMPTY_HASH {
                buffer.push((self.key(i as usize)?, v));
            }
        }

        if !any_change {
            return Ok(LeafUpdate::Unchanged);
        }

        // Slow path: foreign-create. Finalize buffer for synthesis.
        if needs_synthesis {
            if new_witness_value.is_none() && leaf.value_hash != EMPTY_HASH {
                buffer.push((leaf_key, &leaf.value_hash));
            }
            buffer.sort_by(|a, b| a.0.cmp(b.0));
            return Ok(LeafUpdate::Multiple);
        }

        // Fast path: leaf stays single-key. Use the new value if Present wrote, else pre-state.
        Ok(LeafUpdate::Single(new_witness_value.unwrap_or(&leaf.value_hash)))
    }

    /// Hashes the subtree rooted at `depth` containing `entries` sorted by key sharing a prefix.
    fn subtree_hash<H: Hasher>(entries: &[(&[u8; 32], &[u8; 32])], depth: u16) -> HashedNode {
        // Empty subtree - the universal empty summary.
        if entries.is_empty() {
            return HashedNode::EMPTY;
        }

        // Single entry - emit as a shortcut leaf at this depth.
        if entries.len() == 1 {
            return HashedNode::leaf::<H>(entries[0].0, entries[0].1);
        }

        // Multiple entries - split by the bit at `depth` and combine the halves (with promotion).
        let mid = entries.partition_point(|(k, _)| !k.get_msb(depth as usize));
        let (left, right) = entries.split_at(mid);

        HashedNode::combine::<H>(
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
    #[inline]
    fn key(&self, idx: usize) -> Result<&'a [u8; 32]> {
        self.keys.get(idx).ok_or(Error::Decode("key out of range"))
    }

    /// Bounds-checked access to `leaves[idx]`.
    #[inline]
    fn leaf(&self, idx: usize) -> Result<&'a Leaf> {
        self.leaves.get(idx).ok_or(Error::Decode("leaf out of range"))
    }

    /// Bounds-checked access to the leaf's own key (i.e. `keys[leaf.key_idx]`).
    #[inline]
    fn leaf_key(&self, leaf: &Leaf) -> Result<&'a [u8; 32]> {
        self.key(leaf.key_idx.get() as usize)
    }
}
