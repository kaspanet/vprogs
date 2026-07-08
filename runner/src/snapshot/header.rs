//! Typed header embedded in a snapshot container's opaque header bytes, carrying the identity and
//! settlement point a restored node resumes from.
//!
//! On-wire layout: a zerocopy fixed prefix ([`HeaderFixed`]) followed by a length-delimited
//! borsh-encoded [`ChainBlockMetadata`] region. `ChainBlockMetadata` carries an
//! `Option<SettlementInfo>`, which has no zerocopy representation, so only the prefix is zerocopy.

use borsh::BorshDeserialize;
use vprogs_l1_types::{ChainBlockMetadata, Hash, SettlementInfo};
use zerocopy::{
    FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned,
    little_endian::{U32, U64},
};

/// Fixed-size, zerocopy prefix of an encoded [`SnapshotHeader`]. Followed by `cbm_len` bytes of
/// borsh-encoded [`ChainBlockMetadata`].
#[repr(C)]
#[derive(Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout, Unaligned)]
struct HeaderFixed {
    /// Covenant id the snapshot was taken from.
    covenant_id: [u8; 32],
    /// Bootstrap transaction id of the covenant.
    bootstrap_txid: [u8; 32],
    /// Lane id (subnetwork namespace) the snapshot was taken from.
    lane_id: U32,
    /// Batch index the snapshot's records were reconstructed at.
    committed_index: U64,
    /// Length in bytes of the borsh-encoded `ChainBlockMetadata` region following this prefix.
    cbm_len: U32,
}

/// Metadata that seeds a fresh node from a snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotHeader {
    /// Covenant id the snapshot was taken from.
    pub covenant_id: Hash,
    /// Lane id (subnetwork namespace) the snapshot was taken from.
    pub lane_id: u32,
    /// Bootstrap transaction id of the covenant.
    pub bootstrap_txid: Hash,
    /// Batch index the snapshot's records were reconstructed at.
    pub committed_index: u64,
    /// Committed batch metadata at `committed_index`, with `last_settlement` overridden to the
    /// settlement this snapshot pins to. Becomes the restored node's `last_committed` metadata.
    pub chain_block_metadata: ChainBlockMetadata,
}

impl SnapshotHeader {
    /// Encodes the header for embedding in a snapshot container: the zerocopy [`HeaderFixed`]
    /// prefix followed by the borsh-encoded `chain_block_metadata`.
    pub fn encode(&self) -> Vec<u8> {
        let cbm = borsh::to_vec(&self.chain_block_metadata)
            .expect("chain block metadata serialization is infallible");
        let cbm_len: u32 =
            cbm.len().try_into().expect("chain block metadata encodes to under 4 GiB");
        let fixed = HeaderFixed {
            covenant_id: self.covenant_id.as_bytes(),
            bootstrap_txid: self.bootstrap_txid.as_bytes(),
            lane_id: U32::new(self.lane_id),
            committed_index: U64::new(self.committed_index),
            cbm_len: U32::new(cbm_len),
        };
        let mut out = fixed.as_bytes().to_vec();
        out.extend_from_slice(&cbm);
        out
    }

    /// Decodes a header previously produced by [`encode`](Self::encode). Never panics on malformed
    /// input; any length or parse failure is reported as `io::ErrorKind::InvalidData`.
    pub fn decode(bytes: &[u8]) -> Result<Self, std::io::Error> {
        let invalid = |msg: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, msg);

        let (fixed, rest) = HeaderFixed::read_from_prefix(bytes)
            .map_err(|_| invalid("snapshot header shorter than the fixed prefix"))?;

        let cbm_len = fixed.cbm_len.get() as usize;
        if rest.len() != cbm_len {
            return Err(invalid("snapshot header cbm_len does not match trailing bytes"));
        }
        let chain_block_metadata = ChainBlockMetadata::try_from_slice(rest)
            .map_err(|e| invalid(&format!("snapshot header chain_block_metadata: {e}")))?;

        Ok(SnapshotHeader {
            covenant_id: Hash::from_bytes(fixed.covenant_id),
            lane_id: fixed.lane_id.get(),
            bootstrap_txid: Hash::from_bytes(fixed.bootstrap_txid),
            committed_index: fixed.committed_index.get(),
            chain_block_metadata,
        })
    }

    /// The settlement this snapshot pins to (always `Some` for a valid snapshot).
    pub fn settlement(&self) -> Option<SettlementInfo> {
        self.chain_block_metadata.last_settlement
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrips_via_zerocopy_prefix() {
        let meta = ChainBlockMetadata {
            hash: Hash::from_bytes([9u8; 32]),
            blue_score: 123,
            ..ChainBlockMetadata::default()
        };
        let header = SnapshotHeader {
            covenant_id: Hash::from_bytes([1u8; 32]),
            lane_id: 5,
            bootstrap_txid: Hash::from_bytes([2u8; 32]),
            committed_index: 4242,
            chain_block_metadata: meta,
        };
        let bytes = header.encode();
        let back = SnapshotHeader::decode(&bytes).unwrap();
        assert_eq!(back.covenant_id, header.covenant_id);
        assert_eq!(back.lane_id, header.lane_id);
        assert_eq!(back.bootstrap_txid, header.bootstrap_txid);
        assert_eq!(back.committed_index, header.committed_index);
        assert_eq!(back.chain_block_metadata, header.chain_block_metadata);
    }

    #[test]
    fn decode_rejects_truncated_prefix() {
        let err = SnapshotHeader::decode(&[0u8; 10]).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn decode_rejects_cbm_len_mismatch() {
        let meta = ChainBlockMetadata::default();
        let header = SnapshotHeader {
            covenant_id: Hash::from_bytes([1u8; 32]),
            lane_id: 5,
            bootstrap_txid: Hash::from_bytes([2u8; 32]),
            committed_index: 1,
            chain_block_metadata: meta,
        };
        let mut bytes = header.encode();
        bytes.pop(); // truncate the borsh region by one byte
        let err = SnapshotHeader::decode(&bytes).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
