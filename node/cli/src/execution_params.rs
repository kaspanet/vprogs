use clap::Args;
use serde::{Deserialize, Serialize};
use vprogs_scheduling_scheduler::{ExecutionConfig, VmInterface};

use crate::backend::Vm;

#[derive(Args, Serialize, Deserialize)]
#[command(next_help_heading = "Execution")]
pub struct ExecutionParams {
    /// Number of execution worker threads.
    #[arg(long = "execution-worker-count", default_value_t = ExecutionConfig::<Vm>::default().worker_count)]
    pub worker_count: usize,
}

impl ExecutionParams {
    pub fn to_config<V: VmInterface>(self, vm: V) -> ExecutionConfig<V> {
        ExecutionConfig { worker_count: self.worker_count, vm: Some(vm) }
    }
}
