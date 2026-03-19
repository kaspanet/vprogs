use tap::Tap;
use vprogs_core_utils::{BitsArray, Parser, Result};

/// Identifies a position in the tree.
///
/// Level 0 is the root, level 256 is a full-depth leaf. The path encodes left/right decisions
/// (0 = left, 1 = right); only the first `level` bits are significant.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    /// Depth from root (0 = root, 256 = full-depth leaf).
    pub level: u16,
    /// Canonical path - only bits `[0, level)` are meaningful.
    pub path: [u8; 32],
}

impl Key {
    /// The root node position (level 0, all-zero path).
    pub const ROOT: Self = Self { level: 0, path: [0u8; 32] };

    /// Decodes from 34 bytes produced by `encode`, advancing `buf` past the consumed bytes.
    pub fn decode(buf: &mut &[u8]) -> Result<Self> {
        Ok(Self { path: *buf.array::<32>("path")?, level: buf.be_u16("level")? })
    }

    /// Encodes as `path(32) + level(2 BE)` = 34 bytes.
    pub fn encode(&self) -> [u8; 34] {
        [0u8; 34].tap_mut(|buf| {
            buf[..32].copy_from_slice(&self.path);
            buf[32..34].copy_from_slice(&self.level.to_be_bytes());
        })
    }

    /// Returns the left child key (bit 0 at the current level).
    pub(crate) fn left_child(&self) -> Self {
        Self { level: self.level + 1, path: self.path }
    }

    /// Returns the right child key (bit 1 at the current level).
    pub(crate) fn right_child(&self) -> Self {
        Self { level: self.level + 1, path: self.path.with_bit_set(self.level as usize) }
    }
}
