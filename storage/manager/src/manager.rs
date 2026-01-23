use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use vprogs_core_macros::smart_pointer;
use vprogs_storage_types::Store;

use crate::{ReadCmd, StorageConfig, WriteCmd, read::ReadManager, write::WriteManager};

#[smart_pointer]
pub struct StorageManager<S: Store, R: ReadCmd<S::StateSpace>, W: WriteCmd<S::StateSpace>> {
    store: Arc<S>,
    reader: ReadManager<S, R>,
    writer: WriteManager<S, W>,
    is_shutdown: Arc<AtomicBool>,
}

impl<S: Store, R: ReadCmd<S::StateSpace>, W: WriteCmd<S::StateSpace>> StorageManager<S, R, W> {
    pub fn new(config: StorageConfig<S>) -> Self {
        let (store, write_config, read_config) = config.unpack();

        let store = Arc::new(store);
        let is_shutdown = Arc::new(AtomicBool::new(false));

        Self(Arc::new(StorageManagerData {
            reader: ReadManager::new(read_config, &store, &is_shutdown),
            writer: WriteManager::new(write_config, &store, &is_shutdown),
            store,
            is_shutdown,
        }))
    }

    pub fn submit_read(&self, cmd: R) {
        self.reader.submit(cmd);
    }

    pub fn submit_write(&self, cmd: W) {
        self.writer.submit(cmd);
    }

    pub fn store(&self) -> &Arc<S> {
        &self.store
    }

    pub fn shutdown(&self) {
        self.is_shutdown.store(true, Ordering::Release);

        self.reader.shutdown();
        self.writer.shutdown();
    }
}
