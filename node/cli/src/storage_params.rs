use std::path::PathBuf;

use clap::Args;
use serde::Deserialize;

#[derive(Args, Deserialize)]
#[command(next_help_heading = "Storage")]
pub struct StorageParams {
    /// RocksDB data directory.
    #[arg(long, default_value = "./data")]
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,

    /// Maximum number of concurrent read workers.
    #[arg(long, default_value_t = 8)]
    #[serde(default = "default_max_readers")]
    pub max_readers: usize,

    /// Per-reader request buffer depth.
    #[arg(long, default_value_t = 128)]
    #[serde(default = "default_buffer_depth_per_reader")]
    pub buffer_depth_per_reader: usize,

    /// Maximum number of writes batched into a single RocksDB commit.
    #[arg(long, default_value_t = 1000)]
    #[serde(default = "default_max_batch_size")]
    pub max_batch_size: usize,

    /// Maximum time (ms) to wait before flushing a write batch.
    #[arg(long, default_value_t = 10)]
    #[serde(default = "default_max_batch_duration_ms")]
    pub max_batch_duration_ms: u64,
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("./data")
}

fn default_max_readers() -> usize {
    8
}

fn default_buffer_depth_per_reader() -> usize {
    128
}

fn default_max_batch_size() -> usize {
    1000
}

fn default_max_batch_duration_ms() -> u64 {
    10
}
