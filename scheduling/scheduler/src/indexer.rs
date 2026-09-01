//! App-defined secondary indexes maintained inside the state write path.
//!
//! A [`ResourceIndexer`] decodes resource wire bytes and appends secondary-index
//! entries into [`StateSpace::Index`], inside the same WriteBatch as the state itself, so
//! index and state can never disagree on disk. Methods are no-ops by default: an indexer
//! that does not recognize a resource kind simply writes nothing.

use std::sync::Arc;

use vprogs_core_types::ResourceId;
use vprogs_storage_types::WriteBatch;

/// Hook surface for secondary indexes over the resource store.
///
/// All methods run on the storage write worker. `old`/`new`/`restored` carry the resource's
/// wire bytes before/after the diff (`None` = absent); `version` is the committing batch's
/// checkpoint index.
pub trait ResourceIndexer: Send + Sync + 'static {
    /// Append-only event entries for a resource diff (index A).
    fn index_events(
        &self,
        _id: &ResourceId,
        _old: Option<&[u8]>,
        _new: Option<&[u8]>,
        _version: u64,
        _wb: &mut dyn WriteBatch,
    ) {
    }

    /// Current-snapshot bucket rewrite for a resource diff (index B).
    fn index_state(
        &self,
        _id: &ResourceId,
        _old: Option<&[u8]>,
        _new: Option<&[u8]>,
        _version: u64,
        _wb: &mut dyn WriteBatch,
    ) {
    }

    /// Snapshot-index rollback: clear and restore `id`'s entries given the restored bytes.
    fn revert_state(
        &self,
        _id: &ResourceId,
        _restored: Option<&[u8]>,
        _version: u64,
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
