use vprogs_storage_types::{StateSpace, Store, WriteBatch};

/// Provides type-safe operations for the CanonicalChain column family.
pub struct CanonicalChain;

impl CanonicalChain {
    /// Records the canonical block hash for a batch index.
    pub fn set<W: WriteBatch>(wb: &mut W, batch_index: u64, block_hash: &[u8; 32]) {
        wb.put(StateSpace::CanonicalChain, &batch_index.to_be_bytes(), block_hash);
    }

    /// Deletes the canonical entry for a single batch index.
    pub fn delete<W: WriteBatch>(wb: &mut W, batch_index: u64) {
        wb.delete(StateSpace::CanonicalChain, &batch_index.to_be_bytes());
    }

    /// Iterates all recorded entries in ascending batch-index order.
    pub fn iter<S: Store>(store: &S) -> impl Iterator<Item = (u64, [u8; 32])> + '_ {
        // Empty prefix scans the whole CF; fixed-width big-endian keys yield ascending index order.
        store.prefix_iter(StateSpace::CanonicalChain, &[]).map(|(key, value)| {
            let index = u64::from_be_bytes(key[..8].try_into().expect("corrupted canonical key"));
            let block_hash: [u8; 32] = value[..32].try_into().expect("corrupted canonical value");
            (index, block_hash)
        })
    }
}
