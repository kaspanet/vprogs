use kaspa_rpc_core::RpcError;

use crate::BlockHash;

/// Bridge error types, split into recoverable (RPC) and fatal.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    /// Recoverable RPC/network error — will retry on reconnect.
    #[error("RPC error: {0}")]
    Rpc(RpcError),

    /// The starting block has been pruned or reorged away.
    #[error("starting block no longer in chain: {0}")]
    CheckpointLost(RpcError),

    /// A reorg would roll back past the finalization boundary.
    #[error("reorg of {num_blocks} blocks would roll back past finalization boundary")]
    RollbackPastRoot { num_blocks: u64 },

    /// A pruning point hash was not found in the virtual chain.
    #[error("pruning point hash {0} not found in chain")]
    HashNotFound(BlockHash),

    /// The recovery target hash was not found during gap recovery.
    #[error("recovery target hash not found in chain")]
    RecoveryTargetNotFound,

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
    /// Classifies RPC errors by inspecting the message text. The Kaspa RPC
    /// library does not expose structured error variants, so string matching
    /// is the only option for now.
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
