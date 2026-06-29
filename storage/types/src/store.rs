use vprogs_core_smt::Tree;
use vprogs_core_types::BatchMetadata;
use vprogs_storage_canonical_chain::{CanonicalChain, CanonicalChainManager};

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

    /// Returns the store's shared canonical-chain read oracle.
    fn canonical_chain(&self) -> CanonicalChain;

    /// Restores a single-owner manager over this store's oracle, each id being its stored index.
    fn canonical_chain_manager<M: BatchMetadata>(&self) -> CanonicalChainManager<M> {
        // Decode each committed batch, taking its id from the storage key.
        let entries = self.prefix_iter(StateSpace::BatchMetadata, &[]).map(|(key, value)| {
            let id = u64::from_be_bytes(key[..8].try_into().expect("corrupted batch index key"));
            let metadata: M = borsh::from_slice(&value).expect("corrupted batch metadata");
            (id, metadata)
        });

        // Replay them into a manager over this store's oracle.
        CanonicalChainManager::new(self.canonical_chain(), entries)
    }
}
