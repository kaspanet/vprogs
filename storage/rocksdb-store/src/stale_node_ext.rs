use tap::Tap;
use vprogs_core_smt::{Key, StaleNode};
use vprogs_core_utils::{Parser, Result};

/// Storage-specific encoding for `StaleNode` in the SmtStale column family.
///
/// The CF key layout is `stale_since_version(8 BE) || key.encode()(34)` = 42 bytes, with the
/// 8-byte prefix extractor grouping all stale markers for the same version. The CF value holds
/// `node_version(8 BE)`.
pub trait StaleNodeExt {
    /// Encodes the CF key: `stale_since_version(8 BE) || node_key.encode()(34)` = 42 bytes.
    fn encode_key(&self) -> [u8; 42];

    /// Encodes the CF value: `node_version(8 BE)` = 8 bytes.
    fn encode_value(&self) -> [u8; 8];

    /// Decodes the node key from a raw 42-byte SmtStale CF key.
    fn decode_key(raw_key: &[u8]) -> Result<Key>;

    /// Decodes the node version from a raw SmtStale CF value.
    fn decode_value(raw_value: &[u8]) -> Result<u64>;
}

impl StaleNodeExt for StaleNode {
    fn encode_key(&self) -> [u8; 42] {
        [0u8; 42].tap_mut(|buf| {
            buf[..8].copy_from_slice(&self.stale_since_version.to_be_bytes());
            buf[8..42].copy_from_slice(&self.key.encode());
        })
    }

    fn encode_value(&self) -> [u8; 8] {
        self.version.to_be_bytes()
    }

    fn decode_key(raw_key: &[u8]) -> Result<Key> {
        Key::decode((&mut &*raw_key).skip(8, "stale_since_version")?)
    }

    fn decode_value(raw_value: &[u8]) -> Result<u64> {
        (&mut &*raw_value).be_u64("node_version")
    }
}
