use vprogs_core_types::{BatchMetadata, Checkpoint};
use vprogs_state_metadata::StateMetadata;
use vprogs_state_space::StateSpace;
use vprogs_storage_manager::{ReadCmd, StorageManager, WriteCmd};
use vprogs_storage_types::Store;

/// Extension trait that provides a `last_checkpoint` method on [`StorageManager`].
///
/// This reads the last processed checkpoint directly from disk, making it explicit at the call
/// site that a disk read is involved (via the turbofish on the metadata type).
pub trait StorageExt {
    fn last_checkpoint<M: BatchMetadata>(&self) -> Checkpoint<M>;
}

impl<S, R, W> StorageExt for StorageManager<S, R, W>
where
    S: Store<StateSpace = StateSpace>,
    R: ReadCmd<StateSpace>,
    W: WriteCmd<StateSpace>,
{
    fn last_checkpoint<M: BatchMetadata>(&self) -> Checkpoint<M> {
        StateMetadata::last_processed(&**self.store())
    }
}
