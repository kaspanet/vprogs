use tap::Tap;
use vprogs_core_utils::{DecodeError, Parser};

/// Identifies a position in the binary Sparse Merkle Tree.
///
/// At `level` 0 this is the root. At `level` 256 this is a leaf. The `path` field encodes the
/// left/right decisions from root to this node (0 = left, 1 = right), with only the first `level`
/// bits being significant; the rest are zero (canonical form).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    /// Depth from root (0 = root, 256 = leaf level).
    pub level: u16,
    /// Canonical path — only bits `[0, level)` are meaningful.
    pub path: [u8; 32],
}

impl Key {
    /// The root node position.
    pub fn root() -> Self {
        Self { level: 0, path: [0u8; 32] }
    }

    /// Encodes as `path(32) || level(2 BE)` = 34 bytes.
    pub fn encode(&self) -> [u8; 34] {
        [0u8; 34].tap_mut(|buf| {
            buf[..32].copy_from_slice(&self.path);
            buf[32..34].copy_from_slice(&self.level.to_be_bytes());
        })
    }

    /// Decodes from bytes produced by `encode`, advancing `buf` past the consumed bytes.
    pub fn decode(buf: &mut &[u8]) -> Result<Self, DecodeError> {
        Ok(Self { path: *buf.consume_array::<32>("path")?, level: buf.consume_u16_be("level")? })
    }
}
