use alloc::vec::Vec;

/// CSR-flat inversion of memberships by proof-leaf position.
pub(crate) struct MembersByLeaf {
    /// Half-open offset ranges into `indices`; length `leaves.len() + 1`.
    pub(crate) offsets: Vec<u32>,
    /// Membership indices grouped by leaf, ordered by `offsets`.
    pub(crate) indices: Vec<u32>,
}

impl MembersByLeaf {
    /// Returns the membership indices landing at proof-leaf position `leaf_pos`.
    #[inline]
    pub(crate) fn at(&self, leaf_pos: usize) -> &[u32] {
        let start = self.offsets[leaf_pos] as usize;
        let end = self.offsets[leaf_pos + 1] as usize;
        &self.indices[start..end]
    }
}
