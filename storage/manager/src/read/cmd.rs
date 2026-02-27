use vprogs_storage_types::ReadStore;

pub trait ReadCmd: Send + Sync + 'static {
    fn exec<S: ReadStore>(&self, store: &S);
}
