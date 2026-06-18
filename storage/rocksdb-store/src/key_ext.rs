use tap::Tap;
use vprogs_core_codec::{Reader, Result};
use vprogs_core_smt::Key;

/// Storage-specific versioned encoding for `Key` in the SmtNode column family.
///
/// Layout: `key.encode()(34) || !version(8 BE) || block_hash(32)` = 74 bytes. The 34-byte prefix
/// extractor groups all entries for the same node. The `!version` suffix sorts higher versions
/// first, so a forward seek from `!max_version` hits the latest version <= `max_version` first; the
/// trailing `block_hash` lets competing forks store distinct entries at the same version, which a
/// fork-aware read filters by canonicality.
pub trait KeyExt {
    /// Encodes the 42-byte seek prefix `key.encode()(34) || !version(8 BE)`.
    ///
    /// A forward seek from this prefix lands on the highest-versioned entry <= `version` for this
    /// node (across all forks), since `block_hash` only orders entries that share a version.
    fn encode_seek_prefix(&self, version: u64) -> [u8; 42];

    /// Encodes the full node key `key.encode()(34) || !version(8 BE) || block_hash(32)` = 74 bytes.
    fn encode_with_version(&self, version: u64, block_hash: &[u8; 32]) -> [u8; 74];

    /// Decodes the inverted version from a raw SmtNode CF key, skipping the 34-byte key prefix.
    fn decode_version(raw_key: &[u8]) -> Result<u64>;

    /// Decodes the trailing block_hash from a raw SmtNode CF key.
    fn decode_block_hash(raw_key: &[u8]) -> Result<[u8; 32]>;
}

impl KeyExt for Key {
    fn encode_seek_prefix(&self, version: u64) -> [u8; 42] {
        [0u8; 42].tap_mut(|buf| {
            buf[..34].copy_from_slice(&self.encode());
            buf[34..42].copy_from_slice(&(!version).to_be_bytes());
        })
    }

    fn encode_with_version(&self, version: u64, block_hash: &[u8; 32]) -> [u8; 74] {
        [0u8; 74].tap_mut(|buf| {
            buf[..42].copy_from_slice(&self.encode_seek_prefix(version));
            buf[42..74].copy_from_slice(block_hash);
        })
    }

    fn decode_version(raw_key: &[u8]) -> Result<u64> {
        Ok(!(&mut &*raw_key).skip(34, "key")?.be_u64("version")?)
    }

    fn decode_block_hash(raw_key: &[u8]) -> Result<[u8; 32]> {
        Ok(*(&mut &*raw_key).skip(42, "key")?.array::<32>("block_hash")?)
    }
}
