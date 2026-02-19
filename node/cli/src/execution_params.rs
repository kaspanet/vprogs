use clap::Args;
use serde::{Deserialize, Serialize};
use vprogs_scheduling_scheduler::{ExecutionConfig, VmInterface};

use crate::backend::Vm;

/// CLI arguments for the execution worker pool.
#[derive(Args, Serialize, Deserialize)]
#[command(next_help_heading = "Execution")]
pub struct ExecutionParams {
    /// Number of execution worker threads (defaults to physical CPU count).
    #[arg(long = "execution-worker-count", default_value_t = ExecutionConfig::<Vm>::default().worker_count)]
    pub worker_count: usize,
}

impl ExecutionParams {
    /// Converts CLI params into an [`ExecutionConfig`], injecting the given VM implementation.
    pub fn into_config<V: VmInterface>(self, vm: V) -> ExecutionConfig<V> {
        ExecutionConfig { worker_count: self.worker_count, vm: Some(vm) }
    }
}
