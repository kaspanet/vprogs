use tap::Tap;
use vprogs_core_smt::Key;
use vprogs_core_utils::{Parser, Result};

/// Storage-specific versioned encoding for `Key` in the SmtNode column family.
///
/// Layout: `key.encode()(34) || !version(8 BE)` = 42 bytes. The 34-byte prefix extractor groups
/// all versions of the same node. The `!version` suffix sorts higher versions first, so a forward
/// seek from `!max_version` hits the latest version <= `max_version` first.
pub trait KeyExt {
    /// Encodes as `key.encode()(34) || !version(8 BE)` = 42 bytes.
    fn encode_with_version(&self, version: u64) -> [u8; 42];

    /// Decodes the inverted version from a raw SmtNode CF key, skipping the 34-byte key prefix.
    fn decode_version(raw_key: &[u8]) -> Result<u64>;
}

impl KeyExt for Key {
    fn encode_with_version(&self, version: u64) -> [u8; 42] {
        [0u8; 42].tap_mut(|buf| {
            buf[..34].copy_from_slice(&self.encode());
            buf[34..42].copy_from_slice(&(!version).to_be_bytes());
        })
    }

    fn decode_version(raw_key: &[u8]) -> Result<u64> {
        Ok(!(&mut &*raw_key).skip(34, "key")?.be_u64("version")?)
    }
}
