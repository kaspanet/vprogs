use crate::TREE_DEPTH;

/// Identifies a position in the binary Sparse Merkle Tree.
///
/// At `bit_pos` 0 this is the root. At `bit_pos` 256 this is a leaf. The `path` field encodes the
/// left/right decisions from root to this node (0 = left, 1 = right), with only the first `bit_pos`
/// bits being significant; the rest are zero (canonical form).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct NodeKey {
    /// Depth from root (0 = root, 256 = leaf level).
    pub bit_pos: u16,
    /// Canonical path — only bits `[0, bit_pos)` are meaningful.
    pub path: [u8; 32],
}

impl NodeKey {
    /// The root node position.
    pub fn root() -> Self {
        Self { bit_pos: 0, path: [0u8; 32] }
    }

    /// A leaf node position for the given key.
    pub fn leaf(key: [u8; 32]) -> Self {
        Self { bit_pos: TREE_DEPTH as u16, path: key }
    }

    /// Constructs a canonical node key for a given depth, using the first `bit_pos` bits of `key`.
    ///
    /// Bits beyond `bit_pos` are zeroed to maintain canonical form.
    pub fn at(bit_pos: u16, key: &[u8; 32]) -> Self {
        let mut path = *key;
        clear_bits_from(&mut path, bit_pos as usize);
        Self { bit_pos, path }
    }

    /// Returns the left child's node key.
    ///
    /// Left means bit=0 at the current depth, which is already zero in canonical form.
    pub fn left_child(&self) -> Self {
        Self { bit_pos: self.bit_pos + 1, path: self.path }
    }

    /// Returns the right child's node key.
    ///
    /// Right means bit=1 at the current depth — set that bit in the path.
    pub fn right_child(&self) -> Self {
        let mut path = self.path;
        set_key_bit(&mut path, self.bit_pos as usize);
        Self { bit_pos: self.bit_pos + 1, path }
    }

    /// Encodes this node key for the SmtNode column family.
    ///
    /// Key layout: `path(32) || bit_pos(2 BE) || !version(8 BE)` = 42 bytes. The `!version`
    /// suffix sorts higher versions first within the same (path, bit_pos) prefix, so a forward
    /// seek from `!max_version` hits the latest version first.
    pub fn encode_cf_key(&self, version: u64) -> [u8; 42] {
        let mut key = [0u8; 42];
        key[..32].copy_from_slice(&self.path);
        key[32..34].copy_from_slice(&self.bit_pos.to_be_bytes());
        key[34..42].copy_from_slice(&(!version).to_be_bytes());
        key
    }

    /// Builds a 42-byte seek key for `prefix_iter` to find the latest version <= `max_version`.
    ///
    /// The 34-byte prefix extractor groups all versions of the same node.
    pub fn encode_seek_key(&self, max_version: u64) -> [u8; 42] {
        self.encode_cf_key(max_version)
    }

    /// Decodes the version from a raw 42-byte SmtNode key.
    pub fn decode_version(raw_key: &[u8]) -> u64 {
        let inv = u64::from_be_bytes(raw_key[34..42].try_into().unwrap());
        !inv
    }
}

/// Returns the `bit_pos`-th bit of a 256-bit key (0 = MSB).
///
/// MSB-first ordering matches the tree's left/right convention: bit=0 goes left, bit=1 goes right.
pub(crate) fn get_key_bit(key: &[u8; 32], bit_pos: usize) -> bool {
    let byte_idx = bit_pos / 8;
    let bit_offset = 7 - (bit_pos % 8); // MSB-first within each byte.
    (key[byte_idx] >> bit_offset) & 1 == 1
}

/// Sets the `bit_pos`-th bit of a 256-bit key (0 = MSB).
pub(crate) fn set_key_bit(key: &mut [u8; 32], bit_pos: usize) {
    let byte_idx = bit_pos / 8;
    let bit_offset = 7 - (bit_pos % 8); // MSB-first within each byte.
    key[byte_idx] |= 1 << bit_offset;
}

/// Clears all bits at positions >= `from_bit` (MSB-first ordering).
///
/// Used to produce canonical node keys where only the first `bit_pos` bits are meaningful.
pub(crate) fn clear_bits_from(key: &mut [u8; 32], from_bit: usize) {
    if from_bit >= 256 {
        return;
    }
    let byte_idx = from_bit / 8;
    let bit_in_byte = from_bit % 8;
    if bit_in_byte == 0 {
        // Clear from the start of this byte onward.
        for b in &mut key[byte_idx..] {
            *b = 0;
        }
    } else {
        // Keep the upper `bit_in_byte` bits of this byte, clear the rest.
        key[byte_idx] &= 0xFF << (8 - bit_in_byte);
        for b in &mut key[byte_idx + 1..] {
            *b = 0;
        }
    }
}
