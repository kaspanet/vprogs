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

    /// A reorg's fork point sits below the sink root (the seed anchor or the finalized floor), so
    /// there is no tracked block to roll back to.
    #[error("reorg below the sink root: fork block {0} is not tracked")]
    ReorgBelowRoot(Hash),

    /// The peer elided a response field required at `Full` verbosity - retrying cannot help.
    #[error("malformed RPC response: {0}")]
    MalformedResponse(String),

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

/// Whether `e` is the node's lane-proof error for a query that bottoms out at the chain's
/// genesis, where no selected parent exists to walk and no lane state can have accumulated: the
/// authoritative answer is the zero lane state, not a failure.
pub fn lane_walk_reached_genesis(e: &RpcError) -> bool {
    e.to_string().contains("is genesis and has no selected parent")
}

/// Convenience alias used throughout the bridge worker.
pub(crate) type Result<T> = std::result::Result<T, Error>;
