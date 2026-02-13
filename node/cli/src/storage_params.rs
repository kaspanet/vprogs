use std::{path::PathBuf, time::Duration};

use clap::Args;
use serde::{Deserialize, Serialize};
use vprogs_storage_manager::{ReadConfig, StorageConfig, WriteConfig};
use vprogs_storage_types::Store;

#[derive(Args, Serialize, Deserialize)]
#[command(next_help_heading = "Storage")]
pub struct StorageParams {
    /// RocksDB data directory [default: ./data].
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<PathBuf>,

    /// Maximum number of concurrent read workers [default: 8].
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_readers: Option<usize>,

    /// Per-reader request buffer depth [default: 128].
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buffer_depth_per_reader: Option<usize>,

    /// Maximum number of writes batched into a single RocksDB commit [default: 1000].
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_batch_size: Option<usize>,

    /// Maximum time (ms) to wait before flushing a write batch [default: 10].
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_batch_duration_ms: Option<u64>,
}

impl Default for StorageParams {
    fn default() -> Self {
        let default_read_config = ReadConfig::default();
        let default_write_config = WriteConfig::default();
        Self {
            data_dir: Some(PathBuf::from("./data")),
            max_readers: Some(default_read_config.max_readers),
            buffer_depth_per_reader: Some(default_read_config.buffer_depth_per_reader),
            max_batch_size: Some(default_write_config.max_batch_size),
            max_batch_duration_ms: Some(default_write_config.max_batch_duration.as_millis() as u64),
        }
    }
}

impl<S: Store> From<StorageParams> for StorageConfig<S> {
    fn from(params: StorageParams) -> Self {
        Self::default()
            .with_read_config(
                ReadConfig::default()
                    .with_max_readers(params.max_readers.expect("max_readers"))
                    .with_buffer_depth_per_reader(
                        params.buffer_depth_per_reader.expect("buffer_depth_per_reader"),
                    ),
            )
            .with_write_config(
                WriteConfig::default()
                    .with_max_batch_size(params.max_batch_size.expect("max_batch_size"))
                    .with_max_batch_duration(Duration::from_millis(
                        params.max_batch_duration_ms.expect("max_batch_duration_ms"),
                    )),
            )
    }
}
