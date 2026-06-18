use vprogs_core_hashing::Hasher;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use crate::EMPTY_HASH;

/// Kind discriminant for an empty subtree.
pub const EMPTY: u8 = 0;
/// Kind discriminant for a shortcut leaf.
pub const LEAF: u8 = 1;
/// Kind discriminant for an internal node.
pub const INTERNAL: u8 = 2;

/// Structural summary of a child node as seen by its parent during hashing.
///
/// Pairs the child's structural kind with its 32-byte hash so that the parent's hash can commit to
/// both. Authenticating kinds through the root lets a verifier safely apply shortcut-promotion
/// during post-state computation.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Unaligned)]
pub struct HashedNode {
    /// Kind discriminant: `0` = Empty, `1` = Leaf, `2` = Internal.
    pub kind: u8,
    /// The 32-byte hash; equals `EMPTY_HASH` when `kind == 0`.
    pub hash: [u8; 32],
}

impl HashedNode {
    /// The empty-subtree sentinel.
    pub const EMPTY: Self = Self { kind: EMPTY, hash: EMPTY_HASH };

    /// Constructs a summary from a `kind` discriminant and its 32-byte hash.
    pub fn new(kind: u8, hash: [u8; 32]) -> Self {
        Self { kind, hash }
    }

    /// Domain-separated leaf summary of `(key, value_hash)`.
    pub fn leaf<H: Hasher>(key: &[u8; 32], value_hash: &[u8; 32]) -> Self {
        match value_hash {
            &EMPTY_HASH => Self::EMPTY,
            value_hash => Self::new(LEAF, H::hash_parts_with_domain(&[LEAF], [key, value_hash])),
        }
    }

    /// Domain-separated internal-node summary of two child summaries, committing both kinds.
    pub fn internal<H: Hasher>(left: &Self, right: &Self) -> Self {
        // Perform subtree compression if both children are empty.
        if left.kind == EMPTY && right.kind == EMPTY {
            return Self::EMPTY;
        }

        Self::new(
            INTERNAL,
            H::hash_parts_with_domain(
                &[INTERNAL, left.kind, right.kind],
                [&left.hash, &right.hash],
            ),
        )
    }

    /// Combines two child summaries into their parent's summary, applying shortcut-promotion.
    pub fn combine<H: Hasher>(left: &Self, right: &Self) -> Self {
        match (left.kind, right.kind) {
            (LEAF, EMPTY) => *left,
            (EMPTY, LEAF) => *right,
            _ => Self::internal::<H>(left, right),
        }
    }
}
