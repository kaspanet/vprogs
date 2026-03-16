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
    mod key;
    mod leaf;
    mod node;
    mod proof;
    mod stale_node;
    mod state_commitment;
    mod store;
    mod write_batch;

    pub use key::Key;
    pub use leaf::Leaf;
    pub use node::Node;
    pub use proof::Proof;
    pub use stale_node::StaleNode;
    pub use state_commitment::StateCommitment;
    pub use store::Store;
    pub use write_batch::WriteBatch;
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
