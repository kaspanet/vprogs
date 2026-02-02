pub use kaspa_hashes::Hash;
use kaspa_hashes::Hash as BlockHash;
pub use kaspa_rpc_core::{RpcOptionalHeader, RpcOptionalTransaction};

/// A position in the chain: block hash and sequential index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChainCoordinate(BlockHash, u64);

impl ChainCoordinate {
    /// Creates a new chain coordinate.
    pub fn new(hash: BlockHash, index: u64) -> Self {
        Self(hash, index)
    }

    /// Returns the block hash.
    pub fn hash(&self) -> BlockHash {
        self.0
    }

    /// Returns the sequential index.
    pub fn index(&self) -> u64 {
        self.1
    }
}

/// Events emitted by the L1 bridge.
#[derive(Clone, Debug)]
pub enum L1Event {
    /// Connection to L1 node established.
    Connected,
    /// Connection to L1 node lost.
    Disconnected,
    /// A chain block was added (in order, past to present).
    ///
    /// Contains the chain block header and all transactions accepted by this chain block.
    /// The accepted transactions are the transactions from this block's mergeset that
    /// became confirmed when this block was added to the selected parent chain.
    ChainBlockAdded {
        /// Sequential index of this chain block, relative to the starting point.
        index: u64,
        /// The chain block header.
        header: Box<RpcOptionalHeader>,
        /// Transactions accepted by this chain block (from its mergeset).
        accepted_transactions: Vec<RpcOptionalTransaction>,
    },
    /// Rollback to a previous state (the coordinate stays, later ones removed).
    Rollback(ChainCoordinate),
    /// Blocks up to this coordinate are now finalized (pruning point advanced on L1).
    /// The scheduler can safely prune state up to and including this index.
    Finalized(ChainCoordinate),
    /// Bridge has completed initial sync and is now streaming live.
    Synced,
    /// Bridge encountered a fatal error and stopped.
    Fatal {
        /// Descriptive message about what happened.
        reason: String,
    },
}
