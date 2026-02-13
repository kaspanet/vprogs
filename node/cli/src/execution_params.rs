use clap::Args;
use serde::Deserialize;

#[derive(Args, Deserialize)]
#[command(next_help_heading = "Execution")]
pub struct ExecutionParams {
    /// Number of execution worker threads (defaults to physical CPU count).
    #[arg(long)]
    pub workers: Option<usize>,
}
