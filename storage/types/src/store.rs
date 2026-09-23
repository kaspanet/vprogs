use std::array::from_fn;

use vprogs_core_smt::Tree;
use vprogs_core_types::BatchMetadata;
use vprogs_storage_canonical_chain::{
    BucketWords, CanonicalChain, CanonicalChainManager, FrozenBits,
};

use crate::{StateSpace, WriteBatch};

/// A boxed iterator over key-value pairs returned by prefix iteration.
pub type PrefixIterator<'a> = Box<dyn Iterator<Item = (Vec<u8>, Vec<u8>)> + 'a>;

/// Versioned key-value persistence, partitioned into [`StateSpace`] column families.
pub trait Store: Tree + Clone + Send + Sync + 'static {
    type WriteBatch: WriteBatch;

    /// Reads the value stored at `key` in `state_space`, or `None` if absent.
    fn get(&self, state_space: StateSpace, key: &[u8]) -> Option<Vec<u8>>;

    /// Opens a new write batch.
    fn write_batch(&self) -> Self::WriteBatch;

    /// Atomically commits a write batch.
    fn commit(&self, write_batch: Self::WriteBatch);

    /// Iterates `(key, value)` pairs in `state_space` whose keys start with `prefix`, in key order.
    ///
    /// # Panics
    /// Panics if the underlying storage operation fails.
    fn prefix_iter(&self, state_space: StateSpace, prefix: &[u8]) -> PrefixIterator<'_>;

    /// Iterates `(key, value)` pairs whose keys start with `prefix`, in reverse key order.
    ///
    /// # Panics
    /// Panics if the underlying storage operation fails.
    fn prefix_iter_rev(&self, state_space: StateSpace, prefix: &[u8]) -> PrefixIterator<'_>;

    /// Iterates `(key, value)` pairs in `state_space` within the half-open range `[start, end)`,
    /// in key order.
    ///
    /// # Panics
    /// Panics if the underlying storage operation fails.
    fn range_iter(&self, state_space: StateSpace, start: &[u8], end: &[u8]) -> PrefixIterator<'_>;

    /// Returns the store's shared canonical-chain read oracle.
    fn canonical_chain(&self) -> CanonicalChain;

    /// Queues the frozen-bit rows into `wb`, keyed by bucket number, overwriting any earlier
    /// row for the same bucket.
    fn put_frozen_bits(&self, wb: &mut Self::WriteBatch, frozen: &[FrozenBits]) {
        for row in frozen {
            wb.put(StateSpace::CanonicalBits, &row.bucket.to_be_bytes(), &encode_words(&row.words));
        }
    }

    /// Persists the canonical bits frozen by a finalization step in one commit.
    fn persist_frozen_bits(&self, frozen: &[FrozenBits]) {
        // An empty step freezes nothing; skip the commit.
        if frozen.is_empty() {
            return;
        }

        let mut wb = self.write_batch();
        self.put_frozen_bits(&mut wb, frozen);
        self.commit(wb);
    }

    /// Restores a single-owner manager over this store's oracle, each id being its stored index.
    fn canonical_chain_manager<M: BatchMetadata>(&self) -> CanonicalChainManager<M> {
        // Decode each committed batch, taking its id from the storage key.
        let entries = self.prefix_iter(StateSpace::BatchMetadata, &[]).map(|(key, value)| {
            let id = u64::from_be_bytes(key[..8].try_into().expect("corrupted batch index key"));
            let metadata: M = borsh::from_slice(&value).expect("corrupted batch metadata");
            (id, metadata)
        });

        // Replay them, plus any frozen bits earlier finalizations persisted, into a manager.
        let frozen =
            self.prefix_iter(StateSpace::CanonicalBits, &[]).map(|(key, value)| FrozenBits {
                bucket: u64::from_be_bytes(
                    key[..8].try_into().expect("corrupted canonical-bits key"),
                ),
                words: decode_words(&value),
            });
        CanonicalChainManager::new_with_frozen(self.canonical_chain(), entries, frozen)
    }
}

/// Big-endian bytes of one bucket's words.
fn encode_words(words: &BucketWords) -> Vec<u8> {
    words.iter().flat_map(|word| word.to_be_bytes()).collect()
}

/// Decodes one bucket's words from big-endian bytes.
fn decode_words(value: &[u8]) -> BucketWords {
    let bytes: &[u8; size_of::<BucketWords>()] =
        value.try_into().expect("corrupted canonical-bits value");
    from_fn(|w| u64::from_be_bytes(bytes[w * 8..w * 8 + 8].try_into().expect("word slice")))
}
