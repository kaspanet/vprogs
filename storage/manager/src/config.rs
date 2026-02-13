use vprogs_storage_types::Store;

use crate::{read::ReadConfig, write::WriteConfig};

#[derive(Clone, Debug)]
pub struct StorageConfig<S: Store> {
    pub read_config: ReadConfig,
    pub write_config: WriteConfig,
    pub store: Option<S>,
}

impl<S: Store> Default for StorageConfig<S> {
    fn default() -> Self {
        Self {
            read_config: ReadConfig::default(),
            write_config: WriteConfig::default(),
            store: None,
        }
    }
}

impl<S: Store> StorageConfig<S> {
    pub fn with_read_config(mut self, read_config: ReadConfig) -> Self {
        self.read_config = read_config;
        self
    }

    pub fn with_write_config(mut self, write_config: WriteConfig) -> Self {
        self.write_config = write_config;
        self
    }

    pub fn with_store(mut self, store: S) -> Self {
        self.store = Some(store);
        self
    }
}
