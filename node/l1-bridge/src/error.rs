use kaspa_hashes::Hash as BlockHash;
use kaspa_rpc_core::RpcError;

/// Error type for L1 bridge operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Recoverable RPC/network error — will retry on reconnect.
    #[error("RPC error: {0}")]
    Rpc(RpcError),

    /// The starting block has been pruned or reorged out of the chain.
    #[error("starting block no longer in chain: {0}")]
    CheckpointLost(RpcError),

    /// A reorg would roll back past the finalization boundary.
    #[error("reorg of {num_blocks} blocks would roll back past finalization boundary")]
    RollbackPastRoot { num_blocks: u64 },

    /// A pruning point hash was not found walking the virtual chain.
    #[error("pruning point hash {0} not found in chain")]
    HashNotFound(BlockHash),

    /// The recovery target hash was not found during gap recovery.
    #[error("recovery target hash not found in chain")]
    RecoveryTargetNotFound,
}

impl Error {
    /// Returns `true` if this error is fatal and the bridge must stop.
    pub fn is_fatal(&self) -> bool {
        !matches!(self, Error::Rpc(_))
    }
}

impl From<RpcError> for Error {
    fn from(e: RpcError) -> Self {
        let msg = e.to_string().to_lowercase();
        let is_checkpoint_lost = msg.contains("cannot find")
            || msg.contains("data is missing")
            || msg.contains("not in selected parent chain");

        if is_checkpoint_lost { Error::CheckpointLost(e) } else { Error::Rpc(e) }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
