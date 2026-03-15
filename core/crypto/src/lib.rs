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
    mod leaf_entry;
    mod multi_proof;
    mod node;
    mod node_data;
    mod node_key;
    mod stale_node;
    mod state_commitment;
    mod tree_store;
    mod tree_write_batch;
    mod versioned_tree;

    pub use leaf_entry::LeafEntry;
    pub use multi_proof::MultiProof;
    pub use node::Node;
    pub use node_data::NodeData;
    pub use node_key::NodeKey;
    pub use stale_node::StaleNode;
    pub use state_commitment::StateCommitment;
    pub use tree_store::TreeStore;
    pub use tree_write_batch::TreeWriteBatch;
    pub use versioned_tree::VersionedTree;
}

// Convenience re-exports at crate root.
pub use hashing::{Blake3Hasher, Hasher};
pub use smt::NodeData;

/// Hash representing an empty/deleted leaf or empty subtree at any depth.
///
/// By convention, any subtree with no live occupants hashes to this value. This avoids hashing
/// `hash_internal(EMPTY, EMPTY)` — the constant is returned directly (empty subtree compression).
pub const EMPTY_HASH: [u8; 32] = [0u8; 32];

/// Number of levels in the tree (256-bit keys).
pub const TREE_DEPTH: usize = 256;
