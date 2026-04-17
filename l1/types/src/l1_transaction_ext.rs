use kaspa_consensus_core::hashing::tx::transaction_v1_rest_preimage;

use crate::L1Transaction;

/// Extension trait for [`L1Transaction`] providing ZK-pipeline accessors.
///
/// These methods extract data needed by the ZK proving pipeline without requiring
/// the scheduler to be aware of ZK-specific concerns.
pub trait L1TransactionExt {
    /// Returns the L2 payload — the application data portion of the transaction's `payload`
    /// field, after stripping the borsh-encoded `Vec<AccessMetadata>` prefix.
    fn l2_payload(&self) -> Vec<u8>;

    /// Returns the rest preimage — the serialized L1 transaction fields excluding payload,
    /// signature scripts, and mass commitment.
    ///
    /// The ZK guest hashes this with `TransactionRest` to derive `rest_digest`, and can
    /// parse it to access input outpoints, outputs, and other fields needed for covenant
    /// verification.
    fn rest_preimage(&self) -> Vec<u8>;
}

impl L1TransactionExt for L1Transaction {
    fn l2_payload(&self) -> Vec<u8> {
        use borsh::BorshDeserialize;
        use vprogs_core_types::AccessMetadata;

        let mut cursor: &[u8] = &self.payload;
        match Vec::<AccessMetadata>::deserialize(&mut cursor) {
            Ok(_) => cursor.to_vec(),
            Err(_) => Vec::new(),
        }
    }

    fn rest_preimage(&self) -> Vec<u8> {
        transaction_v1_rest_preimage(self)
    }
}
