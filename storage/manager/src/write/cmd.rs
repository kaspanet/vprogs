use vprogs_storage_types::Store;

pub trait WriteCmd: Send + Sync + 'static {
    fn exec<S: Store>(&self, store: &S, batch: S::WriteBatch) -> S::WriteBatch;

    fn done(self);
}
