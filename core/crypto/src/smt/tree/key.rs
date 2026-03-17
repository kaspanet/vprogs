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

    /// Encodes this node key for the SmtNode column family.
    ///
    /// Key layout: `path(32) || level(2 BE) || !version(8 BE)` = 42 bytes. The 34-byte prefix
    /// extractor groups all versions of the same node. The `!version` suffix sorts higher versions
    /// first within that prefix, so a forward seek from `!max_version` hits the latest version <=
    /// `max_version` first.
    pub fn encode_cf_key(&self, version: u64) -> [u8; 42] {
        let mut key = [0u8; 42];
        key[..32].copy_from_slice(&self.path);
        key[32..34].copy_from_slice(&self.level.to_be_bytes());
        key[34..42].copy_from_slice(&(!version).to_be_bytes());
        key
    }

    /// Decodes the version from a raw 42-byte SmtNode key.
    pub fn decode_version(raw_key: &[u8]) -> u64 {
        let inv = u64::from_be_bytes(raw_key[34..42].try_into().unwrap());
        !inv
    }
}
