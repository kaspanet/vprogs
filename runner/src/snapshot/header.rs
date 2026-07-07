use borsh::{BorshDeserialize, BorshSerialize};
use vprogs_l1_types::{ChainBlockMetadata, Hash, SettlementInfo};

/// Metadata that seeds a fresh node from a snapshot. `chain_block_metadata` is the committed
/// batch metadata at the settlement block: it carries `hash == settlement.containing_block` and
/// `last_settlement == the settlement this snapshot pins to`, and becomes the restored node's
/// `last_committed` metadata so the bridge fetches on top of that block.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct SnapshotHeader {
    pub covenant_id: Hash,
    pub lane_id: u32,
    pub bootstrap_txid: Hash,
    pub committed_index: u64,
    pub chain_block_metadata: ChainBlockMetadata,
}

impl SnapshotHeader {
    pub fn encode(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("snapshot header serialization is infallible")
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, std::io::Error> {
        borsh::from_slice(bytes)
    }

    /// The settlement this snapshot pins to (always Some for a valid snapshot).
    pub fn settlement(&self) -> Option<SettlementInfo> {
        self.chain_block_metadata.last_settlement
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrips_via_borsh() {
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
        assert_eq!(back.committed_index, header.committed_index);
        assert_eq!(back.chain_block_metadata, header.chain_block_metadata);
    }
}
