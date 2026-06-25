//! Single source of truth for the permission tree's domain-separation tags.
//!
//! The on-chain permission redeem script and the off-chain accumulator must hash leaf, branch, and
//! empty positions with byte-identical domains, or a committed exit becomes unspendable (issue
//! #78). Both sides read their domain bytes from the [`PermNode`] enum here: the script pushes
//! `[PermNode::X as u8]` before `OP_SHA256`, and the accumulator's
//! [`NodeTags`](vprogs_core_merkle_tree::NodeTags) hands the same byte to the streaming builder.
//!
//! Domain separation needs only distinct first bytes across the three positions; a 1-byte tag is
//! sufficient. The single shared byte length also lets the permission tree reuse the generic
//! [`StreamingBuilder`](vprogs_core_merkle_tree::StreamingBuilder) with a 1-byte
//! [`NodeTags`](vprogs_core_merkle_tree::NodeTags) instead of a hand-rolled hashing path.
//!
//! This file is `include!`d by `build.rs` so the build-time empty-subtree table and the runtime
//! accumulator compute against one definition. It must therefore reference only
//! `vprogs_core_merkle_tree`, never `crate::`.

/// Hashed position in the permission Merkle tree, encoded as the 1-byte domain tag.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermNode {
    /// Leaf hash: `SHA256(Leaf || spk || amount_le)`.
    Leaf = 0,
    /// Branch hash: `SHA256(Branch || left || right)`.
    Branch = 1,
    /// Empty-subtree hash: `SHA256(Empty)`.
    Empty = 2,
}

/// [`NodeTags`](vprogs_core_merkle_tree::NodeTags) for the permission tree, sourcing each tag byte
/// from [`PermNode`].
pub struct PermTags;

impl vprogs_core_merkle_tree::NodeTags<1> for PermTags {
    const LEAF: &'static [u8; 1] = &[PermNode::Leaf as u8];
    const BRANCH: &'static [u8; 1] = &[PermNode::Branch as u8];
    const EMPTY: &'static [u8; 1] = &[PermNode::Empty as u8];
}
