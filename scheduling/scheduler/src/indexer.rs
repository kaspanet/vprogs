//! App-defined secondary indexes maintained inside the state write path.
//!
//! A [`ResourceIndexer`] decodes resource wire bytes and appends secondary-index
//! entries into [`StateSpace::Index`], inside the same WriteBatch as the state itself, so
//! index and state can never disagree on disk. Methods are no-ops by default: an indexer
//! that does not recognize a resource kind simply writes nothing.

use std::sync::Arc;

use vprogs_core_types::ResourceId;
use vprogs_storage_types::WriteBatch;

/// Hook surface for app-defined secondary indexes over the resource store.
/// Runs on the storage write worker, inside the state WriteBatch.
pub trait ResourceIndexer: Send + Sync + 'static {
    /// Feed one resource diff; maintain any number of indexes from it.
    /// `old`/`new` are the resource's wire bytes before/after the diff
    /// (`None` = absent); `version` is the committing batch's checkpoint
    /// index.
    fn index_diff(
        &self,
        _id: &ResourceId,
        _old: Option<&[u8]>,
        _new: Option<&[u8]>,
        _version: u64,
        _wb: &mut dyn WriteBatch,
    ) {
    }

    /// Undo the index effects of one reverted resource write.
    ///
    /// The forward diff was (`restored` -> `written`) at `reverted_version`;
    /// delete the entries it inserted and re-put the entries the restored
    /// state implies, stamped `restored_version` (0 with `restored = None`
    /// when the resource did not exist before the fork).
    fn revert_diff(
        &self,
        _id: &ResourceId,
        _written: Option<&[u8]>,
        _restored: Option<&[u8]>,
        _reverted_version: u64,
        _restored_version: u64,
        _wb: &mut dyn WriteBatch,
    ) {
    }
}

/// Cloneable handle to a [`ResourceIndexer`] (`Arc` wrapper so config structs stay `Debug`).
#[derive(Clone)]
pub struct Indexer(pub Arc<dyn ResourceIndexer>);

impl std::fmt::Debug for Indexer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Indexer(..)")
    }
}
