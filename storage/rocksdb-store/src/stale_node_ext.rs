use tap::Tap;
use vprogs_core_codec::{Reader, Result};
use vprogs_core_smt::{Key, StaleNode};

/// Storage-specific encoding for `StaleNode` in the SmtStale column family.
///
/// The CF key layout is `stale_since_version(8 BE) || superseded_by_block_hash(32) ||
/// key.encode()(34)` = 74 bytes, with the 8-byte prefix extractor grouping all stale markers for
/// the same version (across forks). The superseding fork's hash keeps competing forks' markers
/// distinct. The CF value holds the superseded node's `version(8 BE) || block_hash(32)`.
pub trait StaleNodeExt {
    /// Encodes the CF key: `stale_since_version(8) || superseded_by_block_hash(32) || key(34)`.
    fn encode_key(&self) -> [u8; 74];

    /// Encodes the CF value: `version(8 BE) || block_hash(32)` = 40 bytes.
    fn encode_value(&self) -> [u8; 40];

    /// Decodes the superseding fork's block_hash from a raw SmtStale CF key.
    fn decode_superseded_by(raw_key: &[u8]) -> Result<[u8; 32]>;

    /// Decodes the node key from a raw SmtStale CF key.
    fn decode_key(raw_key: &[u8]) -> Result<Key>;

    /// Decodes the superseded node's `(version, block_hash)` from a raw SmtStale CF value.
    fn decode_value(raw_value: &[u8]) -> Result<(u64, [u8; 32])>;
}

impl StaleNodeExt for StaleNode {
    fn encode_key(&self) -> [u8; 74] {
        [0u8; 74].tap_mut(|buf| {
            buf[..8].copy_from_slice(&self.stale_since_version.to_be_bytes());
            buf[8..40].copy_from_slice(&self.superseded_by_block_hash);
            buf[40..74].copy_from_slice(&self.key.encode());
        })
    }

    fn encode_value(&self) -> [u8; 40] {
        [0u8; 40].tap_mut(|buf| {
            buf[..8].copy_from_slice(&self.version.to_be_bytes());
            buf[8..40].copy_from_slice(&self.block_hash);
        })
    }

    fn decode_superseded_by(raw_key: &[u8]) -> Result<[u8; 32]> {
        Ok(*(&mut &*raw_key).skip(8, "stale_since_version")?.array::<32>("superseded_by")?)
    }

    fn decode_key(raw_key: &[u8]) -> Result<Key> {
        Key::decode((&mut &*raw_key).skip(40, "stale_since_version || superseded_by")?)
    }

    fn decode_value(raw_value: &[u8]) -> Result<(u64, [u8; 32])> {
        let reader = &mut &*raw_value;
        Ok((reader.be_u64("version")?, *reader.array::<32>("block_hash")?))
    }
}
