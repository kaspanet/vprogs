//! Sparse Merkle Tree - algorithms, data structures, and proofs.
//!
//! Pure `no_std` crate - all types are available without feature gates.

#![no_std]

extern crate alloc;

pub mod hashing {
    pub(crate) mod blake3;
    pub(crate) mod hasher;

    pub use blake3::Blake3;
    pub use hasher::{EMPTY_HASH, Hasher};
}

pub mod proving {
    pub(crate) mod leaf;
    pub(crate) mod proof;
    pub(crate) mod proof_builder;
    pub(crate) mod topology;
    pub(crate) mod traversal;

    pub use leaf::Leaf;
    pub use proof::Proof;
    pub(crate) use proof_builder::ProofBuilder;
    pub(crate) use topology::Topology;
    pub(crate) use traversal::Traversal;
}

pub(crate) mod commitment;
pub(crate) mod key;
pub(crate) mod node;
pub(crate) mod stale_node;
pub(crate) mod tree;
pub(crate) mod updater;
pub(crate) mod write_batch;

pub use commitment::Commitment;
pub use hashing::{Blake3, EMPTY_HASH, Hasher};
pub use key::Key;
pub use node::Node;
pub use stale_node::StaleNode;
pub use tree::{DEPTH, Tree};
pub use write_batch::WriteBatch;
