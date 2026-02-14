use std::{path::PathBuf, time::Duration};

use clap::Args;
use serde::{Deserialize, Serialize};
use vprogs_storage_manager::{ReadConfig, StorageConfig, WriteConfig};
use vprogs_storage_types::Store;

/// CLI arguments for the storage manager (read pool, write batching, data directory).
#[derive(Args, Serialize, Deserialize)]
#[command(next_help_heading = "Storage")]
pub struct StorageParams {
    /// RocksDB data directory.
    #[arg(long = "storage-data-dir", default_value = ".data")]
    pub data_dir: PathBuf,
    /// Maximum number of concurrent read workers.
    #[arg(long = "storage-read-worker-count", default_value_t = ReadConfig::default().worker_count)]
    pub read_worker_count: usize,
    /// Per-reader request buffer depth.
    #[arg(long = "storage-read-buffer-depth", default_value_t = ReadConfig::default().worker_buffer_depth)]
    pub read_buffer_depth: usize,
    /// Maximum number of writes batched into a single RocksDB commit.
    #[arg(long = "storage-write-batch-size", default_value_t = WriteConfig::default().batch_size)]
    pub write_batch_size: usize,
    /// Maximum time (ms) to wait before flushing a write batch.
    #[arg(long = "storage-write-batch-duration-ms", default_value_t = WriteConfig::default().batch_duration.as_millis() as u64)]
    pub write_batch_duration_ms: u64,
}

impl StorageParams {
    /// Converts CLI params into a [`StorageConfig`], injecting the given store implementation.
    pub fn into_config<S: Store>(self, store: S) -> StorageConfig<S> {
        StorageConfig {
            read_config: ReadConfig {
                worker_count: self.read_worker_count,
                worker_buffer_depth: self.read_buffer_depth,
            },
            write_config: WriteConfig {
                batch_size: self.write_batch_size,
                batch_duration: Duration::from_millis(self.write_batch_duration_ms),
            },
            store: Some(store),
        }
    }
}
