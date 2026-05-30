use alloc::{vec, vec::Vec};

use tap::Tap;
use vprogs_core_codec::{Bits, Error, Reader, Result, Writer};
use vprogs_core_hashing::Hasher;

use crate::{
    EMPTY_HASH, Node,
    proving::{Leaf, Member, MembersByLeaf, Membership, Traversal},
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
        let (members_by_leaf, touched, values) = self.post_state_index(new_value);

        let mut buf = Vec::new();
        Traversal::new(self, |leaf_pos| {
            let prev = self.leaf_hash::<H>(leaf_pos)?;
            let post = if !touched[leaf_pos] {
                prev
            } else {
                self.new_subtree_hash::<H>(
                    leaf_pos,
                    members_by_leaf.at(leaf_pos),
                    &values,
                    &mut buf,
                )?
            };
            Ok((prev, post))
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

    /// Pre-state hash at proof-leaf position `i`: the existing leaf's `hash_leaf(key, value_hash)`.
    #[inline]
    fn leaf_hash<H: Hasher>(&self, i: usize) -> Result<[u8; 32]> {
        let leaf = self.leaf(i)?;
        Ok(Node::hash_leaf::<H>(self.leaf_key(leaf)?, &leaf.value_hash))
    }

    /// Builds `(members_by_leaf, touched, cached values)`.
    fn post_state_index(
        &self,
        new_value: impl Fn(usize) -> &'a [u8; 32],
    ) -> (MembersByLeaf, Vec<bool>, Vec<&'a [u8; 32]>) {
        let leaves_len = self.leaves.len();
        let mut offsets = vec![0u32; leaves_len + 1];
        let mut touched = vec![false; leaves_len];
        let mut values: Vec<&'a [u8; 32]> = Vec::with_capacity(self.memberships.len());

        // Pass 1: cache values, count members per leaf into `offsets[leaf_pos + 1]`, mark touched.
        for (i, m) in self.memberships.iter().enumerate() {
            let v = new_value(i);
            values.push(v);
            let (is_present, leaf_pos) = match *m {
                Membership::Present(p) => (true, p.get() as usize),
                Membership::Absent(p) => (false, p.get() as usize),
            };
            if leaf_pos >= leaves_len {
                continue; // malformed wire; surfaces downstream as a decode error
            }
            offsets[leaf_pos + 1] += 1;

            // Touched if Present writes a different value or Absent writes non-empty.
            let changed =
                if is_present { v != &self.leaves[leaf_pos].value_hash } else { v != &EMPTY_HASH };
            if changed {
                touched[leaf_pos] = true;
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
            if leaf_pos >= leaves_len {
                continue;
            }
            indices[offsets[leaf_pos] as usize] = i as u32;
            offsets[leaf_pos] += 1;
        }
        for i in (1..=leaves_len).rev() {
            offsets[i] = offsets[i - 1];
        }
        offsets[0] = 0;

        (MembersByLeaf { offsets, indices }, touched, values)
    }

    /// Post-state hash at a touched proof-leaf, given its members and cached values.
    fn new_subtree_hash<H: Hasher>(
        &self,
        leaf_pos: usize,
        member_indices: &[u32],
        values: &[&'a [u8; 32]],
        buffer: &mut Vec<(&'a [u8; 32], &'a [u8; 32])>,
    ) -> Result<[u8; 32]> {
        let leaf = self.leaf(leaf_pos)?;

        // Single pass over members AT this leaf: detect structure change, gather entries.
        buffer.clear();
        let mut new_witness_value: Option<&'a [u8; 32]> = None;
        let mut needs_synthesis = false;
        for &i in member_indices {
            let m = self.memberships[i as usize];
            let v = values[i as usize];
            if matches!(m, Membership::Present(_)) {
                new_witness_value = Some(v);
            } else if v != &EMPTY_HASH {
                needs_synthesis = true;
            }
            if v != &EMPTY_HASH {
                buffer.push((self.key(i as usize)?, v));
            }
        }

        // Slow path: foreign-create. Synthesize the new subtree.
        if needs_synthesis {
            if new_witness_value.is_none() && leaf.value_hash != EMPTY_HASH {
                buffer.push((self.leaf_key(leaf)?, &leaf.value_hash));
            }
            buffer.sort_by(|a, b| a.0.cmp(b.0));
            return Ok(Self::subtree_hash::<H>(buffer.as_slice(), leaf.depth.get()));
        }

        // Fast path: leaf stays single-key. Use the new value if Present wrote, else pre-state.
        let value = new_witness_value.unwrap_or(&leaf.value_hash);
        if value == &EMPTY_HASH {
            Ok(EMPTY_HASH)
        } else {
            Ok(Node::hash_leaf::<H>(self.leaf_key(leaf)?, value))
        }
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
