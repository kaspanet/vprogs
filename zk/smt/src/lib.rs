#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod defaults;
mod multi_proof;
#[cfg(feature = "host")]
mod tree;

pub use multi_proof::{MultiProof, encode_multi_proof};
#[cfg(feature = "host")]
pub use tree::SparseMerkleTree;

/// Hash of an empty/deleted leaf.
pub const EMPTY_LEAF_HASH: [u8; 32] = [0u8; 32];

/// Number of levels in the tree (256-bit keys).
pub const TREE_DEPTH: usize = 256;
