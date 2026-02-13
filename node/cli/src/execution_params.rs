use clap::Args;
use serde::{Deserialize, Serialize};
use vprogs_node_vm::VM;
use vprogs_scheduling_scheduler::{ExecutionConfig, VmInterface};

#[derive(Args, Serialize, Deserialize)]
#[command(next_help_heading = "Execution")]
pub struct ExecutionParams {
    /// Number of execution worker threads [default: physical CPU count].
    #[arg(long = "execution-worker-count")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_count: Option<usize>,
}

impl ExecutionParams {
    pub fn to_config<V: VmInterface>(self, vm: V) -> ExecutionConfig<V> {
        ExecutionConfig { worker_count: self.worker_count.expect("worker_count"), vm: Some(vm) }
    }
}

impl Default for ExecutionParams {
    fn default() -> Self {
        let default_config = ExecutionConfig::<VM>::default();
        Self { worker_count: Some(default_config.worker_count) }
    }
}
