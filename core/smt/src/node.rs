use alloc::vec::Vec;

use vprogs_core_codec::{Error, Reader, Result};
use vprogs_core_hashing::Hasher;
use vprogs_core_types::ResourceId;

use crate::{HashedNode, INTERNAL, LEAF};

/// Data stored at a tree position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    /// Two-child node storing only the combined hash.
    Internal {
        /// Domain-separated hash of the two child summaries.
        hash: [u8; 32],
    },
    /// Shortcut leaf - can sit at any depth, avoiding 256-deep paths for isolated keys.
    Leaf {
        /// The resource this leaf represents.
        key: ResourceId,
        /// Hash of the leaf's value.
        value_hash: [u8; 32],
        /// Domain-separated hash of key and value hash.
        hash: [u8; 32],
    },
}

impl Node {
    /// Creates an internal node from its two child summaries.
    pub fn internal<H: Hasher>(left: &HashedNode, right: &HashedNode) -> Self {
        Node::Internal { hash: HashedNode::internal::<H>(left, right).hash }
    }

    /// Creates a shortcut leaf node from a resource ID and value hash.
    pub fn leaf<H: Hasher>(key: ResourceId, value_hash: [u8; 32]) -> Self {
        Node::Leaf { key, value_hash, hash: HashedNode::leaf::<H>(&key, &value_hash).hash }
    }

    /// Deserializes from bytes produced by `encode`, advancing `buf` past the consumed bytes.
    pub fn decode(buf: &mut &[u8]) -> Result<Self> {
        match buf.byte("tag")? {
            INTERNAL => Ok(Node::Internal { hash: *buf.array::<32>("hash")? }),
            LEAF => Ok(Node::Leaf {
                key: (*buf.array::<32>("key")?).into(),
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
    pub fn encode(&self) -> Vec<u8> {
        match self {
            // Internal: tag(1) + hash(32) = 33 bytes.
            Node::Internal { hash } => [&[INTERNAL], &hash[..]].concat(),
            // Leaf: tag(1) + key(32) + value_hash(32) + hash(32) = 97 bytes.
            Node::Leaf { key, value_hash, hash } => {
                [&[LEAF], &key[..], &value_hash[..], &hash[..]].concat()
            }
        }
    }
}

impl From<&Node> for HashedNode {
    /// Projects a stored [`Node`] to its kinded summary.
    fn from(node: &Node) -> Self {
        match node {
            Node::Internal { hash } => HashedNode::new(INTERNAL, *hash),
            Node::Leaf { hash, .. } => HashedNode::new(LEAF, *hash),
        }
    }
}
