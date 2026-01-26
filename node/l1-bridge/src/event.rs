mod block_added;
mod daa_score_changed;
mod finality_conflict;
mod finality_resolved;
mod virtual_chain_changed;

pub use block_added::BlockAdded;
pub use daa_score_changed::DaaScoreChanged;
pub use finality_conflict::FinalityConflict;
pub use finality_resolved::FinalityResolved;
pub use kaspa_consensus_core::Hash as BlockHash;
pub use virtual_chain_changed::VirtualChainChanged;

/// L1 event types emitted by the bridge.
#[derive(Clone, Debug)]
pub enum L1Event {
    /// Connection to L1 node established.
    Connected,
    /// Connection to L1 node lost.
    Disconnected,
    /// A new block was added to the DAG.
    BlockAdded(BlockAdded),
    /// Virtual chain changed (potential reorg).
    VirtualChainChanged(VirtualChainChanged),
    /// Finality conflict detected.
    FinalityConflict(FinalityConflict),
    /// Finality conflict resolved.
    FinalityResolved(FinalityResolved),
    /// DAA score changed.
    DaaScoreChanged(DaaScoreChanged),
}

impl L1Event {
    /// Returns true if this event indicates a potential reorg.
    pub fn is_reorg(&self) -> bool {
        matches!(self, L1Event::VirtualChainChanged(v) if v.is_reorg())
    }

    /// Returns true if this is a connection state change event.
    pub fn is_connection_event(&self) -> bool {
        matches!(self, L1Event::Connected | L1Event::Disconnected)
    }
}
