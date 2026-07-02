use kaspa_rpc_core::RpcError;
use vprogs_l1_types::Hash;

/// Bridge error types, split into recoverable (RPC) and fatal.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    /// Recoverable RPC/network error - will retry on reconnect.
    #[error("RPC error: {0}")]
    Rpc(RpcError),

    /// The starting block has been pruned or reorged away.
    #[error("starting block no longer in chain: {0}")]
    CheckpointLost(RpcError),

    /// A reorg's fork point has been finalized away, so we cannot roll back to it.
    #[error("reorg below finalization boundary: fork block {0} is finalized")]
    ReorgBelowFinality(Hash),

    /// An internal channel closed unexpectedly.
    #[error("notification channel closed: {0}")]
    ChannelClosed(String),
}

impl Error {
    /// Only `Rpc` errors are recoverable; everything else is fatal.
    pub(crate) fn is_fatal(&self) -> bool {
        !matches!(self, Error::Rpc(_))
    }
}

impl From<RpcError> for Error {
    /// Classifies RPC errors by message text; Kaspa's RPC exposes no structured error variants.
    fn from(e: RpcError) -> Self {
        let msg = e.to_string().to_lowercase();
        let is_checkpoint_lost = msg.contains("cannot find")
            || msg.contains("data is missing")
            || msg.contains("not in selected parent chain");

        if is_checkpoint_lost { Error::CheckpointLost(e) } else { Error::Rpc(e) }
    }
}

/// Convenience alias used throughout the bridge worker.
pub(crate) type Result<T> = std::result::Result<T, Error>;
