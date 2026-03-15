use super::key::Key;

/// A node that was superseded by a newer version.
///
/// Recorded during updates and used by the pruning worker to garbage-collect old node versions
/// that are no longer reachable.
pub struct StaleNode {
    /// The version when this node became stale.
    pub stale_since_version: u64,
    /// The node's position in the tree.
    pub node_key: Key,
    /// The version of the now-stale node.
    pub node_version: u64,
}

impl StaleNode {
    /// Encodes this stale node marker for the SmtStale column family.
    ///
    /// Key layout: `stale_since_version(8 BE) || path(32) || bit_pos(2 BE)` = 42 bytes. The
    /// 8-byte prefix extractor groups all stale markers for the same version.
    pub fn encode_cf_key(&self) -> [u8; 42] {
        let mut key = [0u8; 42];
        key[..8].copy_from_slice(&self.stale_since_version.to_be_bytes());
        key[8..40].copy_from_slice(&self.node_key.path);
        key[40..42].copy_from_slice(&self.node_key.bit_pos.to_be_bytes());
        key
    }

    /// Decodes the path and bit_pos from a raw 42-byte SmtStale key.
    pub fn decode_cf_key(raw_key: &[u8]) -> ([u8; 32], u16) {
        let path: [u8; 32] = raw_key[8..40].try_into().unwrap();
        let bit_pos = u16::from_be_bytes(raw_key[40..42].try_into().unwrap());
        (path, bit_pos)
    }

    /// Decodes the node version from a raw SmtStale value.
    pub fn decode_cf_value(raw_value: &[u8]) -> u64 {
        u64::from_be_bytes(raw_value[..8].try_into().unwrap())
    }
}
