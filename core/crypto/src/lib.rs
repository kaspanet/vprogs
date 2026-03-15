//! Cryptographic primitives and data structures.
//!
//! Guest-side types (hashing, node data) are available in `no_std`. Host-side types (tree
//! mutations, proof generation, store integration) require the `host` feature.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod hashing {
    mod blake3;
    mod hasher;

    pub use blake3::Blake3Hasher;
    pub use hasher::Hasher;
}

pub mod smt {
    mod node_data;

    #[cfg(feature = "host")]
    mod leaf_entry;
    #[cfg(feature = "host")]
    mod multi_proof;
    #[cfg(feature = "host")]
    mod node;
    #[cfg(feature = "host")]
    mod node_key;
    #[cfg(feature = "host")]
    mod stale_node;
    #[cfg(feature = "host")]
    mod state_commitment;
    #[cfg(feature = "host")]
    mod tree_store;
    #[cfg(feature = "host")]
    mod tree_write_batch;
    #[cfg(feature = "host")]
    mod versioned_tree;

    #[cfg(feature = "host")]
    pub use leaf_entry::LeafEntry;
    #[cfg(feature = "host")]
    pub use multi_proof::MultiProof;
    #[cfg(feature = "host")]
    pub use node::Node;
    pub use node_data::NodeData;
    #[cfg(feature = "host")]
    pub use node_key::NodeKey;
    #[cfg(feature = "host")]
    pub use stale_node::StaleNode;
    #[cfg(feature = "host")]
    pub use state_commitment::StateCommitment;
    #[cfg(feature = "host")]
    pub use tree_store::TreeStore;
    #[cfg(feature = "host")]
    pub use tree_write_batch::TreeWriteBatch;
    #[cfg(feature = "host")]
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
