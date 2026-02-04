pub use kaspa_hashes::Hash;
pub use kaspa_rpc_core::{RpcOptionalHeader, RpcOptionalTransaction};

use crate::ChainBlock;

/// Events emitted by the L1 bridge.
#[derive(Clone, Debug)]
pub enum L1Event {
    /// Connection to L1 node established.
    Connected,
    /// Connection to L1 node lost.
    Disconnected,
    /// A new chain block with its accepted transactions.
    ChainBlockAdded {
        /// Sequential index relative to the bridge's starting point.
        index: u64,
        /// Block header.
        header: Box<RpcOptionalHeader>,
        /// Transactions from this block's mergeset that became confirmed.
        accepted_transactions: Vec<RpcOptionalTransaction>,
    },
    /// Blocks after this index have been removed due to a reorg.
    Rollback(u64),
    /// Blocks up to this block are finalized and can be pruned.
    Finalized(ChainBlock),
    /// The bridge encountered a fatal error and stopped.
    Fatal {
        /// What went wrong.
        reason: String,
    },
}
