pub use kaspa_hashes::Hash;
use kaspa_hashes::Hash as BlockHash;
pub use kaspa_rpc_core::RpcBlock;

/// Events emitted by the L2 bridge.
#[derive(Clone, Debug)]
pub enum L2Event {
    /// Connection to L1 node established.
    Connected,
    /// Connection to L1 node lost.
    Disconnected,
    /// A new block was added (in order, past to present).
    BlockAdded {
        /// Sequential index of this block, relative to the starting point.
        index: u64,
        /// The block data.
        block: Box<RpcBlock>,
    },
    /// Rollback to a previous state.
    Rollback {
        /// Index to roll back to (this index stays, later ones removed).
        to_index: u64,
        /// Block hash to roll back to (this block stays, later ones removed).
        to_hash: BlockHash,
    },
    /// Blocks up to this index are now finalized (pruning point advanced on L1).
    /// The scheduler can safely prune state up to and including this index.
    Finalized {
        /// The index up to which blocks are finalized.
        index: u64,
        /// The hash of the finalized block (the new pruning point).
        hash: BlockHash,
    },
    /// Bridge has completed initial sync and is now streaming live.
    Synced,
    /// Unrecoverable error: the starting block is no longer in the chain.
    /// The consumer must restart the bridge from a valid checkpoint.
    SyncLost {
        /// Descriptive message about what happened.
        reason: String,
    },
}
