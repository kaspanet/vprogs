use alloc::vec::Vec;

use tap::Tap;
use vprogs_core_codec::{Error, Reader, Result};

use crate::{EMPTY_HASH, Hasher};

/// Data stored at a tree position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    /// Two-child node storing only the combined hash.
    Internal {
        /// Domain-separated hash of the two child hashes.
        hash: [u8; 32],
    },
    /// Shortcut leaf - can sit at any depth, avoiding 256-deep paths for isolated keys.
    Leaf {
        /// The full 256-bit key this leaf represents.
        key: [u8; 32],
        /// Hash of the leaf's value.
        value_hash: [u8; 32],
        /// Domain-separated hash of key and value hash.
        hash: [u8; 32],
    },
}

impl Node {
    /// Creates an internal node from its two child hashes.
    pub fn internal<H: Hasher>(left: &[u8; 32], right: &[u8; 32]) -> Self {
        Node::Internal { hash: Self::hash_internal::<H>(left, right) }
    }

    /// Creates a shortcut leaf node from a key and value hash.
    pub fn leaf<H: Hasher>(key: [u8; 32], value_hash: [u8; 32]) -> Self {
        Node::Leaf { key, value_hash, hash: Self::hash_leaf::<H>(&key, &value_hash) }
    }

    /// Domain-separated hash of two child hashes (tag `0x00`).
    ///
    /// Returns `EMPTY_HASH` if both children are empty (subtree compression).
    pub fn hash_internal<H: Hasher>(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
        match (left, right) {
            (&EMPTY_HASH, &EMPTY_HASH) => EMPTY_HASH,
            (left, right) => H::hash(&[0u8; 65].tap_mut(|buf| {
                buf[0] = 0x00;
                buf[1..33].copy_from_slice(left);
                buf[33..65].copy_from_slice(right);
            })),
        }
    }

    /// Domain-separated hash of a key and value hash (tag `0x01`).
    ///
    /// Returns `EMPTY_HASH` for deletions (empty value hash).
    pub fn hash_leaf<H: Hasher>(key: &[u8; 32], value_hash: &[u8; 32]) -> [u8; 32] {
        match value_hash {
            &EMPTY_HASH => EMPTY_HASH,
            value_hash => H::hash(&[0u8; 65].tap_mut(|buf| {
                buf[0] = 0x01;
                buf[1..33].copy_from_slice(key);
                buf[33..65].copy_from_slice(value_hash);
            })),
        }
    }

    /// Deserializes from bytes produced by `encode`, advancing `buf` past the consumed bytes.
    pub fn decode(buf: &mut &[u8]) -> Result<Self> {
        match buf.byte("tag")? {
            0x00 => Ok(Node::Internal { hash: *buf.array::<32>("hash")? }),
            0x01 => Ok(Node::Leaf {
                key: *buf.array::<32>("key")?,
                value_hash: *buf.array::<32>("value_hash")?,
                hash: *buf.array::<32>("hash")?,
            }),
            _ => Err(Error::Decode("unknown node tag")),
        }
    }

    /// Returns the hash of this node (internal hash or leaf hash).
    pub fn hash(&self) -> &[u8; 32] {
        match self {
            Node::Internal { hash } => hash,
            Node::Leaf { hash, .. } => hash,
        }
    }

    /// Serializes to bytes for storage.
    ///
    /// Wire format: `tag(1) + fields`. Tag byte matches the domain separation prefix used in
    /// hashing (`0x00` = internal, `0x01` = leaf).
    pub fn encode(&self) -> Vec<u8> {
        match self {
            // Internal: tag(0x00) + hash(32) = 33 bytes.
            Node::Internal { hash } => [&[0x00], hash.as_slice()].concat(),
            // Leaf: tag(0x01) + key(32) + value_hash(32) + hash(32) = 97 bytes.
            Node::Leaf { key, value_hash, hash } => {
                [&[0x01], key.as_slice(), value_hash.as_slice(), hash.as_slice()].concat()
            }
        }
    }
}
