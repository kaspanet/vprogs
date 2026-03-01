use crate::{StateSpace, WriteBatch};

/// A boxed iterator over key-value pairs returned by prefix iteration.
pub type PrefixIterator<'a> = Box<dyn Iterator<Item = (Vec<u8>, Vec<u8>)> + 'a>;

pub trait Store: Send + Sync + 'static {
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
}
