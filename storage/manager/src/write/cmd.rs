use vprogs_storage_types::Store;

/// A command the write worker applies to an accumulating write batch.
pub trait WriteCmd: Send + Sync + 'static {
    /// Applies the command to the batch and returns the batch to continue with.
    fn exec<S: Store>(&self, store: &S, batch: S::WriteBatch) -> S::WriteBatch;

    /// Whether the worker must flush immediately after this command instead of batching it.
    fn flush_now(&self) -> bool;

    /// Called once the command's batch has been committed.
    fn flushed(self);
}
