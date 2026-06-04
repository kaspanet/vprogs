//! Sparse Merkle Tree - algorithms, data structures, and proofs.
//!
//! Pure `no_std` crate - all types are available without feature gates.

#![no_std]

extern crate alloc;

pub mod proving {
    pub(crate) mod leaf;
    pub(crate) mod leaf_update;
    pub(crate) mod member;
    pub(crate) mod members_by_leaf;
    pub(crate) mod membership;
    pub(crate) mod proof;
    pub(crate) mod proof_builder;
    pub(crate) mod topology;
    pub(crate) mod traversal;

    pub use leaf::Leaf;
    pub(crate) use leaf_update::LeafUpdate;
    pub use member::Member;
    pub(crate) use members_by_leaf::MembersByLeaf;
    pub use membership::Membership;
    pub use proof::Proof;
    pub(crate) use proof_builder::ProofBuilder;
    pub(crate) use topology::Topology;
    pub(crate) use traversal::Traversal;
}

pub(crate) mod commitment;
pub(crate) mod empty_hash;
pub(crate) mod hashed_node;
pub(crate) mod key;
pub(crate) mod node;
pub(crate) mod stale_node;
pub(crate) mod tree;
pub(crate) mod updater;
pub(crate) mod write_batch;

pub use commitment::Commitment;
pub use empty_hash::EMPTY_HASH;
pub use hashed_node::{EMPTY, HashedNode, INTERNAL, LEAF};
pub use key::Key;
pub use node::Node;
pub use stale_node::StaleNode;
pub use tree::{DEPTH, Tree};
pub use write_batch::WriteBatch;
