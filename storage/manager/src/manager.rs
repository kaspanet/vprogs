use std::sync::Arc;

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
}

impl<S: Store, R: ReadCmd, W: WriteCmd> StorageManager<S, R, W> {
    /// Opens the store and spawns the read and write workers.
    pub fn new(config: StorageConfig<S>) -> Self {
        let store = Arc::new(config.store.expect("StorageConfig requires a store"));

        Self(Arc::new(StorageManagerData {
            reader: ReadManager::new(config.read_config, &store),
            writer: WriteManager::new(config.write_config, &store),
            store,
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

    /// Stops the read workers, then the write worker, and waits for each to finish.
    ///
    /// Meant to be called once: a second caller at the same time does not wait for the readers the
    /// first one is joining.
    pub fn shutdown(&self) {
        // The write worker is told to stop only after the readers are joined: a read served in
        // their drain can still submit a write, and the write worker has to be running to take it.
        // A write left in the queue would never execute, and it would keep alive whatever it holds.
        self.reader.shutdown();
        self.writer.shutdown();
    }
}
