//! Optimized Sparse Merkle Tree (v2) with leaf shortcutting and domain-separated hashing.
//!
//! Guest-side types (proof verification, hashing, node data) are available in `no_std`. Host-side
//! types (tree mutations, storage, proof generation) require the `host` feature.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod blake3_hasher;
mod decoded_multi_proof;
pub mod hasher;
pub mod node_data;

#[cfg(feature = "host")]
pub mod leaf_entry;
#[cfg(feature = "host")]
mod in_memory_store;
#[cfg(feature = "host")]
mod multi_proof;
#[cfg(feature = "host")]
mod node_key;
#[cfg(feature = "host")]
mod stale_node;
#[cfg(feature = "host")]
mod tree_store;
#[cfg(feature = "host")]
mod tree_update_batch;
#[cfg(feature = "host")]
mod versioned_tree;

pub use blake3_hasher::Blake3Hasher;
pub use decoded_multi_proof::DecodedMultiProof;
pub use hasher::Hasher;
pub use node_data::NodeData;

#[cfg(feature = "host")]
pub use multi_proof::MultiProof;

/// Re-exports of host-side versioned tree types.
#[cfg(feature = "host")]
pub mod versioned {
    pub use crate::{
        in_memory_store::InMemoryStore, node_key::NodeKey, stale_node::StaleNode,
        tree_store::TreeStore, tree_update_batch::TreeUpdateBatch, versioned_tree::VersionedTree,
    };
}

/// Hash representing an empty/deleted leaf or empty subtree at any depth.
///
/// By convention, any subtree with no live occupants hashes to this value. This avoids hashing
/// `hash_internal(EMPTY, EMPTY)` — the constant is returned directly (empty subtree compression).
pub const EMPTY_HASH: [u8; 32] = [0u8; 32];

/// Number of levels in the tree (256-bit keys).
pub const TREE_DEPTH: usize = 256;
