use alloc::vec::Vec;

use vprogs_core_utils::{Error, Parser, Result};

use crate::{EMPTY_HASH, Hasher};

/// Data stored at a tree position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    /// Two-child node storing only the combined hash.
    Internal {
        /// `hash_internal(left, right)`.
        hash: [u8; 32],
    },
    /// Shortcut leaf - can sit at any depth, avoiding 256-deep paths for isolated keys.
    Leaf {
        /// The full 256-bit key this leaf represents.
        key: [u8; 32],
        /// Hash of the leaf's value.
        value_hash: [u8; 32],
        /// Precomputed `hash_leaf(key, value_hash)`.
        hash: [u8; 32],
    },
}

impl Node {
    /// Creates an internal node from its two child hashes.
    ///
    /// Domain tag `0x00` distinguishes internal nodes from leaves (`0x01`). Both children empty
    /// triggers empty subtree compression - returns `EMPTY_HASH` without hashing.
    pub fn internal<H: Hasher>(left: &[u8; 32], right: &[u8; 32]) -> Self {
        if *left == EMPTY_HASH && *right == EMPTY_HASH {
            return Node::Internal { hash: EMPTY_HASH };
        }

        let mut buf = [0u8; 65];
        buf[0] = 0x00;
        buf[1..33].copy_from_slice(left);
        buf[33..65].copy_from_slice(right);
        Node::Internal { hash: H::hash(&buf) }
    }

    /// Creates a shortcut leaf node from a key and value hash.
    ///
    /// Domain tag `0x01` distinguishes leaves from internal nodes (`0x00`). An empty value hash
    /// represents a deletion - returns `EMPTY_HASH` without hashing.
    pub fn leaf<H: Hasher>(key: [u8; 32], value_hash: [u8; 32]) -> Self {
        if value_hash == EMPTY_HASH {
            return Node::Leaf { key, value_hash, hash: EMPTY_HASH };
        }

        let mut buf = [0u8; 65];
        buf[0] = 0x01;
        buf[1..33].copy_from_slice(&key);
        buf[33..65].copy_from_slice(&value_hash);
        Node::Leaf { key, value_hash, hash: H::hash(&buf) }
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

    /// Deserializes from bytes produced by `encode`, advancing `buf` past the consumed bytes.
    pub fn decode(buf: &mut &[u8]) -> Result<Self> {
        // Dispatch on tag byte.
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
}
