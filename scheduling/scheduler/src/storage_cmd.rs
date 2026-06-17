use std::sync::Arc;

use arc_swap::ArcSwapOption;
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_state_proof_receipt::{AggregatorKey, BatchKey, ReceiptKey, TxKey};
use vprogs_storage_manager::{ReadCmd, WriteCmd};
use vprogs_storage_types::{ReadStore, StateSpace, Store, WriteBatch};
use zerocopy::IntoBytes;

use crate::{ResourceAccess, ScheduledBatch, StateDiff, processor::Processor, rollback::Rollback};

/// A typed proof-receipt key: the concrete coordinate for one receipt kind.
///
/// Each variant carries its actual key struct, so the variant alone pins both the stored key bytes
/// ([`as_bytes`](Self::as_bytes)) and the [`ReceiptValue`] variant a stored blob deserializes into.
/// The storage workers carry a `Key` instead of raw bytes plus a separate kind tag, so the two can
/// never drift apart.
pub(crate) enum Key {
    /// A per-transaction receipt key.
    Tx(TxKey),
    /// A per-batch receipt key.
    Batch(BatchKey),
    /// An aggregate (settlement) receipt key.
    Agg(AggregatorKey),
}

impl Key {
    /// The stored key bytes, starting with the shared `Prefix`.
    fn as_bytes(&self) -> &[u8] {
        match self {
            Key::Tx(key) => key.as_bytes(),
            Key::Batch(key) => key.as_bytes(),
            Key::Agg(key) => key.as_bytes(),
        }
    }
}

/// A typed proof-receipt value, tagged by the kind of receipt it is.
///
/// The variant pins the value to the matching [`Processor`] artifact type, so a caller cannot store
/// a transaction receipt under a batch key (or vice versa). The storage workers (de)serialize
/// whichever variant they hold; every receipt kind shares the [`StateSpace::ProofReceipt`] column
/// family.
pub enum ReceiptValue<S: Store, P: Processor<S>> {
    /// A per-transaction receipt.
    Tx(P::TransactionArtifact),
    /// A per-batch receipt.
    Batch(P::BatchArtifact),
    /// An aggregate (settlement) receipt.
    Agg(P::AggregatorArtifact),
}

impl<S: Store, P: Processor<S>> ReceiptValue<S, P> {
    /// Serializes the held receipt to its stored byte form.
    fn to_bytes(&self) -> Vec<u8> {
        match self {
            ReceiptValue::Tx(receipt) => borsh::to_vec(receipt),
            ReceiptValue::Batch(receipt) => borsh::to_vec(receipt),
            ReceiptValue::Agg(receipt) => borsh::to_vec(receipt),
        }
        .expect("failed to serialize receipt")
    }

    /// Deserializes stored bytes into the variant named by `key`.
    fn from_bytes(key: &Key, bytes: &[u8]) -> Self {
        const CORRUPT: &str = "corrupted proof-receipt store";
        match key {
            Key::Tx(_) => ReceiptValue::Tx(borsh::from_slice(bytes).expect(CORRUPT)),
            Key::Batch(_) => ReceiptValue::Batch(borsh::from_slice(bytes).expect(CORRUPT)),
            Key::Agg(_) => ReceiptValue::Agg(borsh::from_slice(bytes).expect(CORRUPT)),
        }
    }

    /// Moves out the per-transaction receipt. Panics if the value is not a [`Tx`](Self::Tx); the
    /// issuer's lookup kind guarantees the variant.
    pub(crate) fn into_tx(self) -> P::TransactionArtifact {
        match self {
            ReceiptValue::Tx(receipt) => receipt,
            _ => unreachable!("receipt kind mismatch"),
        }
    }

    /// Moves out the per-batch receipt. Panics if the value is not a [`Batch`](Self::Batch).
    pub(crate) fn into_batch(self) -> P::BatchArtifact {
        match self {
            ReceiptValue::Batch(receipt) => receipt,
            _ => unreachable!("receipt kind mismatch"),
        }
    }

    /// Moves out the aggregate receipt. Panics if the value is not an [`Agg`](Self::Agg).
    pub(crate) fn into_agg(self) -> P::AggregatorArtifact {
        match self {
            ReceiptValue::Agg(receipt) => receipt,
            _ => unreachable!("receipt kind mismatch"),
        }
    }
}

/// Ties a proof-receipt key type to the receipt it stores: a single typed key erases into the
/// storage workers' [`Key`] ([`into_key`](Self::into_key)) and drives the handle's projection back
/// to the artifact ([`extract`](Self::extract)) and the write worker's serialization
/// ([`wrap`](Self::wrap)). One impl per key type is the single source of truth pairing a key with
/// its [`ReceiptValue`] variant.
pub(crate) trait ReceiptLookup<S: Store, P: Processor<S>>: ReceiptKey {
    /// The concrete receipt artifact this key stores.
    type Artifact;
    /// Erases this key into the type-tagged [`Key`] the storage workers carry.
    fn into_key(self) -> Key;
    /// Projects a served [`ReceiptValue`] to the artifact.
    fn extract(value: ReceiptValue<S, P>) -> Self::Artifact;
    /// Wraps an artifact into its [`ReceiptValue`] variant for storage.
    fn wrap(artifact: Self::Artifact) -> ReceiptValue<S, P>;
}

impl<S: Store, P: Processor<S>> ReceiptLookup<S, P> for TxKey {
    type Artifact = P::TransactionArtifact;
    fn into_key(self) -> Key {
        Key::Tx(self)
    }
    fn extract(value: ReceiptValue<S, P>) -> Self::Artifact {
        value.into_tx()
    }
    fn wrap(artifact: Self::Artifact) -> ReceiptValue<S, P> {
        ReceiptValue::Tx(artifact)
    }
}

impl<S: Store, P: Processor<S>> ReceiptLookup<S, P> for BatchKey {
    type Artifact = P::BatchArtifact;
    fn into_key(self) -> Key {
        Key::Batch(self)
    }
    fn extract(value: ReceiptValue<S, P>) -> Self::Artifact {
        value.into_batch()
    }
    fn wrap(artifact: Self::Artifact) -> ReceiptValue<S, P> {
        ReceiptValue::Batch(artifact)
    }
}

impl<S: Store, P: Processor<S>> ReceiptLookup<S, P> for AggregatorKey {
    type Artifact = P::AggregatorArtifact;
    fn into_key(self) -> Key {
        Key::Agg(self)
    }
    fn extract(value: ReceiptValue<S, P>) -> Self::Artifact {
        value.into_agg()
    }
    fn wrap(artifact: Self::Artifact) -> ReceiptValue<S, P> {
        ReceiptValue::Agg(artifact)
    }
}

/// A proof-receipt lookup served by the read worker, off the issuer's async runtime.
///
/// The read worker deserializes the stored blob into `slot` as the [`ReceiptValue`] named by `key`
/// (left empty on a cache miss) and opens `ready`, so the proving worker that enqueued it awaits
/// the result via [`ReceiptRead`] instead of blocking its single-threaded runtime on a synchronous
/// store read.
pub struct ReadReceipt<S: Store, P: Processor<S>> {
    /// The typed key identifying the receipt to look up.
    key: Key,
    /// Filled with the served [`ReceiptValue`], or left empty on a cache miss.
    slot: Arc<ArcSwapOption<ReceiptValue<S, P>>>,
    /// Opens once the lookup has been served.
    ready: AtomicAsyncLatch,
}

/// Handle to an in-flight [`ReadReceipt`]: await [`resolve`](Self::resolve) for the typed receipt
/// `R`, or `None` on a cache miss. `extract` projects the served [`ReceiptValue`] to the issuer's
/// concrete artifact type.
pub struct ReceiptRead<S: Store, P: Processor<S>, R> {
    /// Shared slot the read worker fills with the served [`ReceiptValue`].
    slot: Arc<ArcSwapOption<ReceiptValue<S, P>>>,
    /// Opens once the lookup has been served.
    ready: AtomicAsyncLatch,
    /// Projects the served [`ReceiptValue`] to the issuer's concrete artifact type.
    extract: fn(ReceiptValue<S, P>) -> R,
}

impl<S: Store, P: Processor<S>> ReadReceipt<S, P> {
    /// Creates a lookup command for the typed `key` together with the typed [`ReceiptRead`] handle
    /// its issuer awaits, projecting the served value through `extract`.
    pub(crate) fn new<R>(
        key: Key,
        extract: fn(ReceiptValue<S, P>) -> R,
    ) -> (Self, ReceiptRead<S, P, R>) {
        let slot = Arc::new(ArcSwapOption::empty());
        let ready = AtomicAsyncLatch::new();
        let handle = ReceiptRead { slot: slot.clone(), ready: ready.clone(), extract };
        (Self { key, slot, ready }, handle)
    }

    /// Serves the lookup: fills `slot` from the store (a miss leaves it empty), then opens `ready`.
    fn read<RS: ReadStore>(&self, store: &RS) {
        if let Some(bytes) = store.get(StateSpace::ProofReceipt, self.key.as_bytes()) {
            self.slot.store(Some(Arc::new(ReceiptValue::from_bytes(&self.key, &bytes))));
        }
        self.ready.open();
    }
}

impl<S: Store, P: Processor<S>, R> ReceiptRead<S, P, R> {
    /// Resolves once the read worker has served the lookup: the typed receipt, or `None` on a cache
    /// miss.
    pub async fn resolve(self) -> Option<R> {
        self.ready.wait().await;
        let value = self.slot.swap(None)?;
        let value = Arc::into_inner(value).expect("receipt slot uniquely held");
        Some((self.extract)(value))
    }
}

/// A typed proof-receipt awaiting durable storage in the [`StateSpace::ProofReceipt`] column
/// family.
///
/// Carries the key bytes, the typed [`ReceiptValue`] (serialized by the write worker), and a latch
/// the worker opens once the write commits, so the proving worker that enqueued it can wait for
/// durability before publishing the matching artifact.
pub struct StoreReceipt<S: Store, P: Processor<S>> {
    /// The typed key the receipt is stored under.
    key: Key,
    /// The typed receipt, serialized by the write worker.
    value: ReceiptValue<S, P>,
    /// Opens once the write commits.
    committed: AtomicAsyncLatch,
}

impl<S: Store, P: Processor<S>> StoreReceipt<S, P> {
    /// Pairs the typed `key` and `value` with the latch opened once the write commits.
    pub(crate) fn new(key: Key, value: ReceiptValue<S, P>, committed: AtomicAsyncLatch) -> Self {
        Self { key, value, committed }
    }

    /// Writes the serialized receipt into `wb` under its key.
    fn write<W: WriteBatch>(&self, wb: &mut W) {
        wb.put(StateSpace::ProofReceipt, self.key.as_bytes(), &self.value.to_bytes());
    }

    /// Opens the `committed` latch once the write has landed.
    fn done(self) {
        self.committed.open();
    }
}

/// Commands dispatched to the storage manager's read worker.
pub enum Read<S: Store, P: Processor<S>> {
    /// Fetch the latest version data for a resource from disk.
    LatestData(ResourceAccess<S, P>),
    /// Look up a cached typed proof-receipt by key.
    ReadReceipt(ReadReceipt<S, P>),
}

impl<S: Store, P: Processor<S>> ReadCmd for Read<S, P> {
    fn exec<RS: ReadStore>(&self, store: &RS) {
        match self {
            Read::LatestData(resource_access) => resource_access.read_latest_data(store),
            Read::ReadReceipt(receipt) => receipt.read(store),
        }
    }
}

/// Commands dispatched to the storage manager's write worker.
pub enum Write<S: Store, P: Processor<S>> {
    /// Persist a resource's versioned data and rollback pointer.
    StateDiff(StateDiff<S, P>),
    /// Finalize a batch by writing latest pointers and batch metadata.
    CommitBatch(ScheduledBatch<S, P>),
    /// Revert all batches after a target checkpoint.
    Rollback(Rollback<S, P>),
    /// Persist a typed proof-receipt in the proof-receipt column family.
    StoreReceipt(StoreReceipt<S, P>),
}

impl<S: Store, P: Processor<S>> WriteCmd for Write<S, P> {
    fn exec<ST: Store>(&self, store: &ST, mut wb: ST::WriteBatch) -> ST::WriteBatch {
        match self {
            Write::StateDiff(state_diff) => state_diff.write(&mut wb),
            Write::CommitBatch(batch) => batch.commit(store, &mut wb),
            Write::Rollback(rollback) => return rollback.execute(store, wb),
            Write::StoreReceipt(receipt) => receipt.write(&mut wb),
        }
        wb
    }

    fn done(self) {
        match self {
            Write::StateDiff(state_diff) => state_diff.write_done(),
            Write::CommitBatch(batch) => batch.commit_done(),
            Write::Rollback(rollback) => rollback.done(),
            Write::StoreReceipt(receipt) => receipt.done(),
        }
    }
}
