//! Optimized Sparse Merkle Tree (v2) with leaf shortcutting and domain-separated hashing.
//!
//! Guest-side types (hashing, node data) are available in `no_std`. Host-side types (tree
//! mutations, storage, proof generation) require the `host` feature. The RocksDB integration layer
//! (key encoding, commit, metadata) also requires `host`.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod blake3_hasher;
pub mod hasher;
pub mod node_data;

#[cfg(feature = "host")]
mod in_memory_store;
#[cfg(feature = "host")]
pub mod leaf_entry;
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

#[cfg(feature = "host")]
mod commit;
#[cfg(feature = "host")]
mod metadata;
#[cfg(feature = "host")]
mod rocksdb_store;

pub use blake3_hasher::Blake3Hasher;
pub use hasher::Hasher;
#[cfg(feature = "host")]
pub use multi_proof::MultiProof;
pub use node_data::NodeData;

/// Re-exports of host-side versioned tree types.
#[cfg(feature = "host")]
pub mod versioned {
    pub use crate::{
        in_memory_store::InMemoryStore, node_key::NodeKey, stale_node::StaleNode,
        tree_store::TreeStore, tree_update_batch::TreeUpdateBatch, versioned_tree::VersionedTree,
    };
}

/// Re-exports of host-side RocksDB integration types.
#[cfg(feature = "host")]
pub mod persistence {
    pub use crate::{commit::SmtCommit, metadata::SmtMetadata, rocksdb_store::RocksDbTreeStore};
}

/// Hash representing an empty/deleted leaf or empty subtree at any depth.
///
/// By convention, any subtree with no live occupants hashes to this value. This avoids hashing
/// `hash_internal(EMPTY, EMPTY)` — the constant is returned directly (empty subtree compression).
pub const EMPTY_HASH: [u8; 32] = [0u8; 32];

/// Number of levels in the tree (256-bit keys).
pub const TREE_DEPTH: usize = 256;
