mod chain_block_metadata;
mod connect_strategy;
mod hash;
mod l1_transaction;
mod l1_transaction_covenant_ext;
mod network_id;
mod network_type;
mod settlement_info;
mod transaction_id;

use std::io::{Read, Write};

use borsh::{BorshDeserialize, BorshSerialize};
pub use chain_block_metadata::ChainBlockMetadata;
pub use connect_strategy::ConnectStrategy;
pub use hash::Hash;
pub use l1_transaction::L1Transaction;
pub use l1_transaction_covenant_ext::L1TransactionCovenantExt;
pub use network_id::NetworkId;
pub use network_type::NetworkType;
pub use settlement_info::SettlementInfo;
pub use transaction_id::TransactionId;

/// Plain-data event emitted when a permission (exit) UTXO is spent on L1.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PermissionSpend {
    /// Covenant id whose permission UTXO was spent.
    pub covenant_id: [u8; 32],
    /// Merkle root of the permission tree before this spend.
    pub old_root: [u8; 32],
    /// Unclaimed exit count before this spend.
    pub old_unclaimed: u64,
    /// Merkle tree depth.
    pub depth: usize,
    /// Index of the spent exit leaf in the tree.
    pub leaf_index: usize,
    /// Script public key bytes of the exit recipient.
    pub leaf_spk_bytes: Vec<u8>,
    /// Total amount allocated to the leaf exit.
    pub leaf_amount: u64,
    /// Amount deducted by this spend.
    pub deduct: u64,
    /// Merkle root of the permission tree after this spend.
    pub new_root: [u8; 32],
    /// L1 transaction id of the spend.
    pub spend_txid: [u8; 32],
    /// Output index of the continuation permission UTXO, if any.
    pub new_outpoint_index: u32,
}

impl BorshSerialize for PermissionSpend {
    fn serialize<W: Write>(&self, writer: &mut W) -> std::io::Result<()> {
        self.covenant_id.serialize(writer)?;
        self.old_root.serialize(writer)?;
        self.old_unclaimed.serialize(writer)?;
        self.depth.serialize(writer)?;
        self.leaf_index.serialize(writer)?;
        self.leaf_spk_bytes.serialize(writer)?;
        self.leaf_amount.serialize(writer)?;
        self.deduct.serialize(writer)?;
        self.new_root.serialize(writer)?;
        self.spend_txid.serialize(writer)?;
        self.new_outpoint_index.serialize(writer)
    }
}

impl BorshDeserialize for PermissionSpend {
    fn deserialize_reader<R: Read>(reader: &mut R) -> std::io::Result<Self> {
        Ok(Self {
            covenant_id: <[u8; 32]>::deserialize_reader(reader)?,
            old_root: <[u8; 32]>::deserialize_reader(reader)?,
            old_unclaimed: u64::deserialize_reader(reader)?,
            depth: usize::deserialize_reader(reader)?,
            leaf_index: usize::deserialize_reader(reader)?,
            leaf_spk_bytes: Vec::<u8>::deserialize_reader(reader)?,
            leaf_amount: u64::deserialize_reader(reader)?,
            deduct: u64::deserialize_reader(reader)?,
            new_root: <[u8; 32]>::deserialize_reader(reader)?,
            spend_txid: <[u8; 32]>::deserialize_reader(reader)?,
            new_outpoint_index: u32::deserialize_reader(reader)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the hand-rolled Borsh impls against field-order drift: distinct values per field so a
    /// serialize/deserialize mismatch would surface as an inequality.
    #[test]
    fn permission_spend_borsh_round_trip() {
        let spend = PermissionSpend {
            covenant_id: [0x11; 32],
            old_root: [0x22; 32],
            old_unclaimed: 100,
            depth: 3,
            leaf_index: 5,
            leaf_spk_bytes: vec![0xaa, 0xbb, 0xcc, 0xdd],
            leaf_amount: 50_000,
            deduct: 10_000,
            new_root: [0x33; 32],
            spend_txid: [0x44; 32],
            new_outpoint_index: 1,
        };
        let bytes = borsh::to_vec(&spend).expect("serialize");
        let decoded = PermissionSpend::try_from_slice(&bytes).expect("deserialize");
        assert_eq!(spend, decoded);
    }
}
