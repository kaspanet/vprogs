use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use vprogs_core_macros::smart_pointer;
use vprogs_storage_types::Store;

use crate::{ReadCmd, StorageConfig, WriteCmd, read::ReadManager, write::WriteManager};

/// Coordinates the read and write workers over a shared store.
#[smart_pointer]
pub struct StorageManager<S: Store, R: ReadCmd, W: WriteCmd> {
    /// Backing store, shared with both workers.
    store: Arc<S>,
    /// Read worker for read commands.
    reader: ReadManager<S, R>,
    /// Write worker for write commands.
    writer: WriteManager<W>,
    /// Set on shutdown to stop both workers.
    is_shutdown: Arc<AtomicBool>,
}

impl<S: Store, R: ReadCmd, W: WriteCmd> StorageManager<S, R, W> {
    /// Opens the store and spawns the read and write workers.
    pub fn new(config: StorageConfig<S>) -> Self {
        let store = Arc::new(config.store.expect("StorageConfig requires a store"));
        let is_shutdown = Arc::new(AtomicBool::new(false));

        Self(Arc::new(StorageManagerData {
            reader: ReadManager::new(config.read_config, &store, &is_shutdown),
            writer: WriteManager::new(config.write_config, &store, &is_shutdown),
            store,
            is_shutdown,
        }))
    }

    /// Queues a read command for the read worker.
    #[inline(always)]
    pub fn submit_read(&self, cmd: R) {
        self.reader.submit(cmd);
    }

    /// Queues a write command for the write worker.
    #[inline(always)]
    pub fn submit_write(&self, cmd: W) {
        self.writer.submit(cmd);
    }

    /// Returns the backing store.
    #[inline(always)]
    pub fn store(&self) -> &Arc<S> {
        &self.store
    }

    /// Signals shutdown and waits for both workers to finish.
    pub fn shutdown(&self) {
        // Flag shutdown before waking the workers so their loops observe it and exit.
        self.is_shutdown.store(true, Ordering::Release);
        self.reader.shutdown();
        self.writer.shutdown();
    }
}
