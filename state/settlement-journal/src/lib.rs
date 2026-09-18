//! Journal of proved-but-unsettled settlement bundles.
//!
//! The aggregate prover records each bundle's geometry here when it publishes the bundle's
//! artifact, and deletes entries once the on-chain covenant tip covers them. A restart reads
//! the surviving entries, reloads each bundle's aggregate receipt from the proof-receipt cache
//! by these coordinates, and re-feeds the bundles to the settlement worker.

use borsh::{BorshDeserialize, BorshSerialize};
use kaspa_hashes::Hash;
use vprogs_l1_types::ChainBlockMetadata;
use vprogs_storage_types::{StateSpace, Store, WriteBatch};

/// One proved-but-unsettled bundle: the geometry needed to reload its aggregate receipt and
/// re-feed it to the settlement worker after a restart.
///
/// `start_index` (the column-family key) and `end_index` pin the bundle's checkpoint span,
/// `from_block` and `seq_commit` complete the receipt-key coordinates, and `block_prove_to`
/// names the L1 block the settlement commits to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct JournalEntry {
    /// Inclusive checkpoint index of the bundle's last batch.
    pub end_index: u64,
    /// L1 block at the bundle's first checkpoint (the block the bundle proves from).
    pub from_block: Hash,
    /// L1 block the bundle proves through (its final batch's block).
    pub block_prove_to: Hash,
    /// Claimed tip commitment at `block_prove_to` (the receipt key's tail coordinate).
    pub seq_commit: Hash,
}

/// Store-backed operations the aggregate prover needs over the settlement journal and the batch
/// metadata it maps boundaries through, as one object so the worker holds no store generic.
pub trait SettlementJournal: Send + Sync {
    /// Records the entry for the bundle starting at `start_index`, replacing any prior one.
    fn record(&self, start_index: u64, entry: &JournalEntry);
    /// Returns all entries ordered by start index.
    fn entries(&self) -> Vec<(u64, JournalEntry)>;
    /// Deletes the entry starting at `start_index`.
    fn delete(&self, start_index: u64);
    /// Returns the chain block hash of batch `index`, or `None` if the batch has no metadata.
    fn batch_block(&self, index: u64) -> Option<Hash>;
    /// Returns the checkpoint index whose batch block is `block`, scanning indices from `upper`
    /// down to `lower` inclusive, or `None` when no batch in the window carries the block.
    ///
    /// A bounded reverse scan: callers bound it by the journal's own span, never the whole chain.
    fn checkpoint_of_block(&self, block: Hash, upper: u64, lower: u64) -> Option<u64>;
    /// Returns whether any entry is recorded.
    fn has_entries(&self) -> bool;
}

/// [`SettlementJournal`] over one store handle.
pub struct StoreJournal<S: Store> {
    /// Store serving the journal and batch-metadata column families.
    store: S,
}

impl<S: Store> StoreJournal<S> {
    /// Wraps a store opened by the proving node.
    pub fn new(store: S) -> Self {
        Self { store }
    }
}

impl<S: Store> SettlementJournal for StoreJournal<S> {
    fn record(&self, start_index: u64, entry: &JournalEntry) {
        let mut wb = self.store.write_batch();
        wb.put(
            StateSpace::SettlementJournal,
            &start_index.to_be_bytes(),
            &borsh::to_vec(entry).expect("serialize JournalEntry"),
        );
        self.store.commit(wb);
    }

    fn entries(&self) -> Vec<(u64, JournalEntry)> {
        self.store
            .prefix_iter(StateSpace::SettlementJournal, &[])
            .map(|(k, v)| {
                let start = u64::from_be_bytes(k.try_into().expect("u64 BE key"));
                let entry = borsh::from_slice(&v).expect("corrupted store: journal entry");
                (start, entry)
            })
            .collect()
    }

    fn delete(&self, start_index: u64) {
        let mut wb = self.store.write_batch();
        wb.delete(StateSpace::SettlementJournal, &start_index.to_be_bytes());
        self.store.commit(wb);
    }

    fn batch_block(&self, index: u64) -> Option<Hash> {
        self.store
            .get(StateSpace::BatchMetadata, &index.to_be_bytes())
            .map(|bytes| borsh::from_slice::<ChainBlockMetadata>(&bytes).expect("corrupted store"))
            .map(|meta| meta.hash)
    }

    fn checkpoint_of_block(&self, block: Hash, upper: u64, lower: u64) -> Option<u64> {
        (lower..=upper).rev().find(|&i| self.batch_block(i) == Some(block))
    }

    fn has_entries(&self) -> bool {
        !self.entries().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use vprogs_l1_types::ChainBlockMetadata;
    use vprogs_storage_rocksdb_store::RocksDbStore;
    use vprogs_storage_types::Store;

    use super::*;

    fn store() -> RocksDbStore {
        RocksDbStore::open(tempfile::TempDir::new().unwrap().path())
    }

    fn entry(end: u64, seed: u8) -> JournalEntry {
        JournalEntry {
            end_index: end,
            from_block: Hash::from_bytes([seed; 32]),
            block_prove_to: Hash::from_bytes([seed + 1; 32]),
            seq_commit: Hash::from_bytes([seed + 2; 32]),
        }
    }

    #[test]
    fn roundtrip_orders_by_start_and_deletes() {
        let s = store();
        let j = StoreJournal::new(s.clone());
        j.record(7, &entry(9, 1));
        j.record(3, &entry(5, 2));
        assert_eq!(j.entries(), vec![(3, entry(5, 2)), (7, entry(9, 1))]);
        assert!(j.has_entries());
        j.delete(3);
        assert_eq!(j.entries(), vec![(7, entry(9, 1))]);
        j.delete(7);
        assert!(j.entries().is_empty());
        assert!(!j.has_entries());
    }

    #[test]
    fn batch_block_and_checkpoint_of_block_roundtrip() {
        let s = store();
        let j = StoreJournal::new(s.clone());
        let meta = ChainBlockMetadata { hash: Hash::from_bytes([0xab; 32]), ..Default::default() };
        let mut wb = s.write_batch();
        vprogs_state_batch_metadata::BatchMetadata::set(&mut wb, 11, &meta);
        s.commit(wb);
        assert_eq!(j.batch_block(11), Some(Hash::from_bytes([0xab; 32])));
        assert_eq!(j.batch_block(12), None);
        assert_eq!(j.checkpoint_of_block(Hash::from_bytes([0xab; 32]), 12, 10), Some(11),);
        // Absent inside the scanned window: miss, not a false hit.
        assert_eq!(j.checkpoint_of_block(Hash::from_bytes([0xcd; 32]), 12, 10), None);
    }
}
