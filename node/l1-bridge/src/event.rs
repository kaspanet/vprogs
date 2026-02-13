pub use kaspa_hashes::Hash;
pub use kaspa_rpc_core::{RpcOptionalHeader, RpcOptionalTransaction};
use vprogs_core_types::Checkpoint;

use crate::ChainBlockMetadata;

/// Events emitted by the L1 bridge.
#[derive(Clone, Debug)]
pub enum L1Event {
    /// Connection to L1 node established.
    Connected,
    /// Connection to L1 node lost.
    Disconnected,
    /// A new chain block with its accepted transactions.
    ChainBlockAdded {
        /// Checkpoint (index + metadata) for this block.
        checkpoint: Checkpoint<ChainBlockMetadata>,
        /// Block header.
        header: Box<RpcOptionalHeader>,
        /// Transactions from this block's mergeset that became confirmed.
        accepted_transactions: Vec<RpcOptionalTransaction>,
    },
    /// Blocks after this checkpoint have been removed due to a reorg.
    Rollback {
        /// Checkpoint of the new tip after the rollback.
        checkpoint: Checkpoint<ChainBlockMetadata>,
        /// Blue score difference between old and new tip.
        blue_score_depth: u64,
    },
    /// Blocks up to this checkpoint are finalized and can be pruned.
    Finalized(Checkpoint<ChainBlockMetadata>),
    /// The bridge encountered a fatal error and stopped.
    Fatal {
        /// What went wrong.
        reason: String,
    },
}
