use vprogs_core_smt::Tree;
use vprogs_core_types::BatchMetadata;
use vprogs_storage_canonical_chain::{CanonicalChain, CanonicalChainManager};

use crate::{StateSpace, WriteBatch};

/// A boxed iterator over key-value pairs returned by prefix iteration.
pub type PrefixIterator<'a> = Box<dyn Iterator<Item = (Vec<u8>, Vec<u8>)> + 'a>;

pub trait Store: Tree + Clone + Send + Sync + 'static {
    type WriteBatch: WriteBatch;

    fn get(&self, state_space: StateSpace, key: &[u8]) -> Option<Vec<u8>>;
    fn write_batch(&self) -> Self::WriteBatch;
    fn commit(&self, write_batch: Self::WriteBatch);

    /// Iterate over all key-value pairs in the given state space whose keys
    /// start with the specified prefix.
    ///
    /// The iterator yields `(key, value)` pairs in lexicographic order of keys.
    /// Iteration stops when a key that does not match the prefix is encountered.
    ///
    /// # Panics
    /// Panics if the underlying storage operation fails.
    fn prefix_iter(&self, state_space: StateSpace, prefix: &[u8]) -> PrefixIterator<'_>;

    /// Returns the store's shared canonical-chain read oracle.
    fn canonical_chain(&self) -> CanonicalChain;

    /// Restores a single-owner canonical-chain manager driving this store's oracle, replaying the
    /// committed `BatchMetadata` so each batch's id equals its stored index.
    fn canonical_chain_manager<M: BatchMetadata>(&self) -> CanonicalChainManager<M> {
        let entries = self.prefix_iter(StateSpace::BatchMetadata, &[]).map(|(key, value)| {
            let id = u64::from_be_bytes(key[..8].try_into().expect("corrupted batch index key"));
            let metadata: M = borsh::from_slice(&value).expect("corrupted batch metadata");
            (id, metadata)
        });
        CanonicalChainManager::new(self.canonical_chain(), entries)
    }
}
