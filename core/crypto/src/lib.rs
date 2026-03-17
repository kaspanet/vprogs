//! Cryptographic primitives and data structures.
//!
//! Pure `no_std` crate — all types are available without feature gates.

#![no_std]

extern crate alloc;

pub mod hashing {
    mod blake3;
    mod hasher;

    pub use blake3::Blake3Hasher;
    pub use hasher::Hasher;
}

pub mod smt {
    pub(crate) mod proof {
        pub(crate) mod builder;
        mod leaf;
        mod multi_proof;
        mod traversal;

        pub use leaf::Leaf;
        pub use multi_proof::Proof;
    }

    pub(crate) mod tree {
        pub(crate) mod key;
        mod node;
        pub(crate) mod stale_node;
        pub(crate) mod state_commitment;
        pub(crate) mod store;
        mod update;
        pub(crate) mod write_batch;

        pub use key::Key;
        pub use node::Node;
        pub use stale_node::StaleNode;
        pub use state_commitment::StateCommitment;
        pub use store::Store;
        pub use write_batch::WriteBatch;
    }

    pub use proof::{Leaf, Proof};
    pub use tree::{Key, Node, StaleNode, StateCommitment, Store, WriteBatch};
}

// Convenience re-exports at crate root.
pub use hashing::{Blake3Hasher, Hasher};
pub use smt::Node;

/// Hash representing an empty/deleted leaf or empty subtree at any depth.
///
/// By convention, any subtree with no live occupants hashes to this value. This avoids hashing
/// `hash_internal(EMPTY, EMPTY)` — the constant is returned directly (empty subtree compression).
pub const EMPTY_HASH: [u8; 32] = [0u8; 32];

/// Number of levels in the tree (256-bit keys).
pub const TREE_DEPTH: usize = 256;
