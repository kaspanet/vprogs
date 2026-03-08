mod account_entry;
mod decoder;
#[cfg(feature = "host")]
mod encoder;
mod header;
mod tx_entry;

pub use account_entry::AccountEntry;
pub use decoder::BatchWitnessDecoder;
#[cfg(feature = "host")]
pub use encoder::encode_batch_witness;
pub use header::BatchWitnessHeader;
pub use tx_entry::{TxEntry, TxEntryIter};

/// Fixed header size for the batch witness:
/// image_id(32) + batch_index(8) + prev_root(32) + n_accounts(4) + n_txs(4).
pub const HEADER_SIZE: usize = 32 + 8 + 32 + 4 + 4;

/// Per-account entry size: resource_id(32) + leaf_hash(32).
pub const ACCOUNT_ENTRY_SIZE: usize = 32 + 32;

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "host")]
    #[test]
    fn roundtrip_encode_decode() {
        use vprogs_zk_smt::encode_multi_proof;

        let image_id = [0xABu8; 32];
        let batch_index = 42u64;
        let prev_root = [0xCDu8; 32];

        let accounts = vec![([1u8; 32], [0x11u8; 32]), ([2u8; 32], [0u8; 32])];

        let multi_proof_bytes = encode_multi_proof(&[], &[], &[]);

        let journal = vec![0xAAu8; 64];
        let wire_bytes = vec![0xBBu8; 128];
        let exec_result = vec![0xCCu8; 32];
        let txs = vec![(journal.clone(), wire_bytes.clone(), exec_result.clone())];

        let encoded = encode_batch_witness(
            &image_id,
            batch_index,
            &prev_root,
            &accounts,
            &multi_proof_bytes,
            &txs,
        );

        let decoder = BatchWitnessDecoder::new(&encoded);
        let header = decoder.header();

        assert_eq!(header.image_id, &image_id);
        assert_eq!(header.batch_index, batch_index);
        assert_eq!(header.prev_root, &prev_root);
        assert_eq!(header.n_accounts, 2);
        assert_eq!(header.n_txs, 1);

        let acc0 = decoder.account_entry(0);
        assert_eq!(acc0.resource_id, &[1u8; 32]);
        assert_eq!(acc0.leaf_hash, &[0x11u8; 32]);

        let acc1 = decoder.account_entry(1);
        assert_eq!(acc1.resource_id, &[2u8; 32]);
        assert_eq!(acc1.leaf_hash, &[0u8; 32]);

        let multi_proof = decoder.multi_proof();
        assert_eq!(multi_proof.n_leaves(), 0);

        let tx_entries: Vec<_> = decoder.tx_entries().collect();
        assert_eq!(tx_entries.len(), 1);
        assert_eq!(tx_entries[0].journal, &journal[..]);
        assert_eq!(tx_entries[0].wire_bytes, &wire_bytes[..]);
        assert_eq!(tx_entries[0].exec_result, &exec_result[..]);
    }
}
