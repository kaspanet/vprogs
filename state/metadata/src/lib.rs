use vprogs_core_smt::EMPTY_HASH;
use vprogs_core_types::{BatchMetadata, Checkpoint};
use vprogs_storage_types::{ReadStore, StateSpace, WriteBatch};

/// Well-known metadata keys.
mod keys {
    /// Key for the root batch (oldest surviving batch; index + metadata stored together).
    pub const ROOT: &[u8] = b"root";
    /// Key for the last committed batch (index + metadata stored together).
    pub const LAST_COMMITTED: &[u8] = b"last_committed";
    /// Key for the 32-byte global state root hash.
    pub const STATE_ROOT: &[u8] = b"smt_root";
}

/// Provides type-safe operations for the Metadata column family.
///
/// StateMetadata stores system-level metadata such as root tracking and commit progress, allowing
/// crash-fault tolerant operations.
pub struct StateMetadata;

impl StateMetadata {
    /// Returns the root checkpoint (oldest surviving batch), or defaults if no batches have been
    /// committed yet.
    ///
    /// The root is initialized when the first batch commits and advances forward as pruning deletes
    /// older batches. A default (index 0) indicates a fresh database with no commits.
    pub fn root<M: BatchMetadata, S>(store: &S) -> Checkpoint<M>
    where
        S: ReadStore,
    {
        store
            .get(StateSpace::Metadata, keys::ROOT)
            .map(|bytes| borsh::from_slice(&bytes).expect("corrupted store: unrecoverable"))
            .unwrap_or_default()
    }

    /// Sets the root checkpoint (oldest surviving batch).
    pub fn set_root<M: BatchMetadata, W>(wb: &mut W, checkpoint: &Checkpoint<M>)
    where
        W: WriteBatch,
    {
        wb.put(
            StateSpace::Metadata,
            keys::ROOT,
            &borsh::to_vec(checkpoint).expect("failed to serialize Checkpoint"),
        );
    }

    /// Returns the last committed batch, or defaults if no batches have been committed yet.
    pub fn last_committed<M: BatchMetadata, S>(store: &S) -> Checkpoint<M>
    where
        S: ReadStore,
    {
        store
            .get(StateSpace::Metadata, keys::LAST_COMMITTED)
            .map(|bytes| borsh::from_slice(&bytes).expect("corrupted store: unrecoverable"))
            .unwrap_or_default()
    }

    /// Sets the last committed batch.
    pub fn set_last_committed<M: BatchMetadata, W>(wb: &mut W, checkpoint: &Checkpoint<M>)
    where
        W: WriteBatch,
    {
        wb.put(
            StateSpace::Metadata,
            keys::LAST_COMMITTED,
            &borsh::to_vec(checkpoint).expect("failed to serialize Checkpoint"),
        );
    }

    /// Returns the current global state root hash, or `EMPTY_HASH` if none has been set.
    pub fn state_root<S: ReadStore>(store: &S) -> [u8; 32] {
        store
            .get(StateSpace::Metadata, keys::STATE_ROOT)
            .map(|bytes| bytes.try_into().expect("corrupted state_root: unrecoverable"))
            .unwrap_or(EMPTY_HASH)
    }

    /// Sets the global state root hash.
    pub fn set_state_root<W: WriteBatch>(wb: &mut W, root: &[u8; 32]) {
        wb.put(StateSpace::Metadata, keys::STATE_ROOT, root);
    }
}
