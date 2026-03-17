use alloc::vec::Vec;

use vprogs_core_utils::{DecodeError, Parser};

/// Data stored at a tree position.
///
/// `Internal` nodes have two children looked up via `Key::left_child()` / `right_child()`.
/// `Leaf` nodes are shortcut leaves that can sit at any depth, storing the full key and value hash
/// to avoid 256-deep paths for isolated keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    Internal {
        hash: [u8; 32],
    },
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
    /// Returns the hash of this node (internal hash or leaf hash).
    pub fn hash(&self) -> &[u8; 32] {
        match self {
            Node::Internal { hash } => hash,
            Node::Leaf { hash, .. } => hash,
        }
    }

    /// Serializes to bytes for storage.
    ///
    /// Tag `0x00` + hash (33B) for Internal, tag `0x01` + key + value_hash + hash (97B) for Leaf.
    /// The tag byte matches the domain separation prefix used in hashing.
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            Node::Internal { hash } => {
                let mut buf = Vec::with_capacity(33);
                buf.push(0x00); // Internal node tag.
                buf.extend_from_slice(hash);
                buf
            }
            Node::Leaf { key, value_hash, hash } => {
                let mut buf = Vec::with_capacity(97);
                buf.push(0x01); // Leaf node tag.
                buf.extend_from_slice(key);
                buf.extend_from_slice(value_hash);
                buf.extend_from_slice(hash);
                buf
            }
        }
    }

    /// Deserializes from bytes produced by `to_bytes`, advancing `buf` past the consumed bytes.
    pub fn decode(buf: &mut &[u8]) -> Result<Self, DecodeError> {
        match buf.consume_u8("tag")? {
            0x00 => Ok(Node::Internal { hash: *buf.consume_array::<32>("hash")? }),
            0x01 => Ok(Node::Leaf {
                key: *buf.consume_array::<32>("key")?,
                value_hash: *buf.consume_array::<32>("value_hash")?,
                hash: *buf.consume_array::<32>("hash")?,
            }),
            _ => Err(DecodeError("unknown node tag")),
        }
    }
}
