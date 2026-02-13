use clap::Args;
use serde::{Deserialize, Serialize};
use vprogs_node_vm::VM;
use vprogs_scheduling_scheduler::{ExecutionConfig, VmInterface};

#[derive(Args, Serialize, Deserialize)]
#[command(next_help_heading = "Execution")]
pub struct ExecutionParams {
    /// Number of execution worker threads [default: physical CPU count].
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workers: Option<usize>,
}

impl Default for ExecutionParams {
    fn default() -> Self {
        let default_config = ExecutionConfig::<VM>::default();
        Self { workers: Some(default_config.worker_count) }
    }
}

impl<V: VmInterface> From<ExecutionParams> for ExecutionConfig<V> {
    fn from(params: ExecutionParams) -> Self {
        Self::default().with_worker_count(params.workers.expect("workers"))
    }
}
