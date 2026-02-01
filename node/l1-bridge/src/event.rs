pub use kaspa_rpc_core::RpcBlock;
use vprogs_node_chain_state::ChainStateCoordinate;

/// Events emitted by the L1 bridge.
#[derive(Clone, Debug)]
pub enum L1Event {
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
    /// Rollback to a previous state (the coordinate stays, later ones removed).
    Rollback(ChainStateCoordinate),
    /// Blocks up to this coordinate are now finalized (pruning point advanced on L1).
    /// The scheduler can safely prune state up to and including this index.
    Finalized(ChainStateCoordinate),
    /// Bridge has completed initial sync and is now streaming live.
    Synced,
    /// Bridge encountered a fatal error and stopped.
    Fatal {
        /// Descriptive message about what happened.
        reason: String,
    },
}
