/// Domain-separation tags for the three hashed positions in a Merkle tree: leaves, internal
/// branches, and empty subtrees.
///
/// The const generic `N` is the tag length in bytes. All three tags share the same length so
/// they slot into the [`Hasher`]'s fixed-size domain parameter. Distinct tag values ensure leaf
/// hashes can't collide with branch hashes (the classic Merkle-tree second-preimage attack) and
/// that empty-subtree hashes form their own domain.
///
/// [`Hasher`]: vprogs_core_hashing::Hasher
pub trait NodeTags<const N: usize> {
    /// Tag for leaf-hash inputs.
    const LEAF: &'static [u8; N];
    /// Tag for branch-hash inputs (parent of two children).
    const BRANCH: &'static [u8; N];
    /// Tag for empty-subtree hashes.
    const EMPTY: &'static [u8; N];
}
