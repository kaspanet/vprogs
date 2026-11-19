use crate::Key;

/// A node that was superseded by a newer version. Used by pruning to garbage-collect old nodes.
///
/// An update marks the node at every position of the prior tree it visits with a commitment,
/// and the node a leaf leaves behind when it moves up. Pruning a canonical version deletes the
/// nodes its markers name. The markers of a version a rollback orphaned name nodes of the history
/// it branched from, so pruning that version drops them unread.
///
/// A tombstone below the root is marked at the version it is written at. Where the update
/// deleted a node at that position, the tombstone's marker replaces the node's. Pruning it as
/// part of a canonical version removes every node at the position up to the tombstone.
#[derive(Debug)]
pub struct StaleNode {
    /// The version when this node became stale.
    pub stale_since_version: u64,
    /// The stale node's position in the tree.
    pub key: Key,
    /// The stale node's version.
    pub version: u64,
}

impl StaleNode {
    /// Creates a new stale node marker.
    pub fn new(stale_since_version: u64, key: Key, version: u64) -> Self {
        Self { stale_since_version, key, version }
    }
}
