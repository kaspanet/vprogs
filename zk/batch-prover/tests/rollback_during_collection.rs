//! Pins what the batch worker does when a rollback lands while it collects a batch's inputs.
//!
//! The worker reads the store twice before it proves: the state root a cached receipt must
//! enter at, and the SMT proof a fresh prove starts from. A rollback that reaches below the
//! version both are read at orphans what they read. The entry check then misreads a receipt
//! that proves the batch as stale, and the proof roots the batch in another lineage than its
//! transaction receipts, which the guest can reject on every retry. The batch is dead either
//! way, so the worker must drop it instead of proving it.
//!
//! A rollback to exactly that version (a re-root to the boundary when the batch has the next
//! id) cancels the batch as well, but leaves what it read intact. Its proof is still worth
//! finishing: the block returns under the same batch id and reuses the receipt. The bound is
//! the version read at, not the batch's parent: with orphaned ids between the two, a sibling
//! fork can return under one of them and change what the batch reads.

// The backend traits return `impl Future + 'static`, which an `async fn` cannot satisfy: its
// future borrows `&self`.
#![allow(clippy::manual_async_fn)]

use std::{
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use kaspa_hashes::Hash;
use tempfile::TempDir;
use tokio::runtime::Builder;
use vprogs_core_smt::{Key, Node, Tree, WriteBatch as SmtWriteBatch};
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId, SchedulerTransaction};
use vprogs_l1_types::ChainBlockMetadata;
use vprogs_scheduling_scheduler::{
    ExecutionConfig, Processor, ScheduledBatch, Scheduler, TransactionContext,
};
use vprogs_storage_canonical_chain::CanonicalChain;
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::{PrefixIterator, StateSpace, Store};
use vprogs_zk_abi::batch_processor::{BatchTransition, BatchTransitionArgs};
use vprogs_zk_batch_prover::{BatchProver, BatchProverConfig};

/// Transaction-guest image id; the processor publishes tx artifacts directly, so it keys nothing.
const TX_IMAGE_ID: [u8; 32] = [0u8; 32];

/// Batch-guest image id, keying the per-batch receipt cache.
const BATCH_IMAGE_ID: [u8; 32] = [1u8; 32];

/// Parks one tree read of a [`ParkingStore`] so a test can act while the read is in flight.
#[derive(Default)]
struct Park {
    /// Set by the test to the number of tree reads until the one that parks. Every read counts
    /// it down, and it is zero while disarmed.
    armed: AtomicUsize,
    /// Set while a read is parked; the test clears it to let the read go on.
    parked: AtomicBool,
}

impl Park {
    /// Arms the park for the `nth` tree read from now, runs `submit`, and blocks until that read
    /// is parked.
    fn until_parked(&self, nth: usize, submit: impl FnOnce()) {
        self.armed.store(nth, Ordering::SeqCst);
        submit();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.parked.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "the worker never reached its tree read");
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// Lets the parked read go on.
    fn release(&self) {
        self.parked.store(false, Ordering::SeqCst);
    }
}

/// A [`Store`] whose tree reads can be parked right before they capture their snapshot. Every
/// operation delegates unchanged, so a parked read observes whatever the store holds once it is
/// released.
#[derive(Clone)]
struct ParkingStore {
    /// The store every operation delegates to.
    inner: RocksDbStore,
    /// Parks the tree read it is armed for.
    park: Arc<Park>,
}

impl Store for ParkingStore {
    type WriteBatch = <RocksDbStore as Store>::WriteBatch;

    fn get(&self, state_space: StateSpace, key: &[u8]) -> Option<Vec<u8>> {
        self.inner.get(state_space, key)
    }

    fn write_batch(&self) -> Self::WriteBatch {
        self.inner.write_batch()
    }

    fn commit(&self, write_batch: Self::WriteBatch) {
        self.inner.commit(write_batch);
    }

    fn prefix_iter(&self, state_space: StateSpace, prefix: &[u8]) -> PrefixIterator<'_> {
        self.inner.prefix_iter(state_space, prefix)
    }

    fn prefix_iter_rev(&self, state_space: StateSpace, prefix: &[u8]) -> PrefixIterator<'_> {
        self.inner.prefix_iter_rev(state_space, prefix)
    }

    fn range_iter(&self, state_space: StateSpace, start: &[u8], end: &[u8]) -> PrefixIterator<'_> {
        self.inner.range_iter(state_space, start, end)
    }

    fn canonical_chain(&self) -> CanonicalChain {
        self.inner.canonical_chain()
    }
}

impl Tree for ParkingStore {
    type Hasher = <RocksDbStore as Tree>::Hasher;
    type Snapshot = <RocksDbStore as Tree>::Snapshot;

    fn snapshot(&self) -> Self::Snapshot {
        // Every root lookup above version 0 and every proof captures its snapshot first, so
        // parking here holds the whole read before it has seen anything.
        let countdown = |reads: usize| reads.checked_sub(1);
        if self.park.armed.try_update(Ordering::SeqCst, Ordering::SeqCst, countdown) == Ok(1) {
            self.park.parked.store(true, Ordering::SeqCst);
            while self.park.parked.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(1));
            }
        }
        self.inner.snapshot()
    }

    fn node(&self, key: &Key, max_version: u64, snapshot: &Self::Snapshot) -> Option<(u64, Node)> {
        self.inner.node(key, max_version, snapshot)
    }

    fn prune(&self, wb: &mut impl SmtWriteBatch, version: u64) {
        self.inner.prune(wb, version)
    }
}

/// A batch receipt entering at `prev_state` and settling `new_state`, with the default lane
/// pins. The backend's `journal_bytes` is the identity, so the receipt is its own journal.
fn receipt(prev_state: [u8; 32], new_state: [u8; 32]) -> Vec<u8> {
    let mut journal = Vec::new();
    BatchTransition::encode(
        &mut journal,
        BatchTransitionArgs {
            prev_state: &prev_state,
            prev_lane_tip: &Hash::default(),
            prev_lane_blue_score: 0,
            new_state: &new_state,
            new_lane_tip: &Hash::default(),
            new_lane_blue_score: 0,
            lane_key: &Hash::default(),
            covenant_id: &[0u8; 32],
            tx_image_id: &TX_IMAGE_ID,
            deposit_spk_hash: &[0u8; 32],
            lane_expired: false,
            exits: &[],
        },
    );
    journal
}

/// Backend that counts every batch prove and hands out the receipt the test scripted.
#[derive(Clone, Default)]
struct CountingBackend {
    /// Proves run so far, shared with the worker's clone.
    proves: Arc<AtomicUsize>,
    /// Receipt a prove returns.
    fresh: Arc<Mutex<Vec<u8>>>,
}

impl vprogs_zk_transaction_prover::Backend for CountingBackend {
    type Receipt = Vec<u8>;

    fn image_id(&self) -> &[u8; 32] {
        &TX_IMAGE_ID
    }

    fn prove_transaction(
        &self,
        _input_bytes: Vec<u8>,
    ) -> impl Future<Output = Result<Vec<u8>, String>> + Send + 'static {
        async { unreachable!("the processor publishes tx artifacts directly") }
    }
}

impl vprogs_zk_batch_prover::Backend for CountingBackend {
    fn prove_batch(
        &self,
        _inputs: &[u8],
        _receipts: Vec<Vec<u8>>,
    ) -> impl Future<Output = Result<Vec<u8>, String>> + Send + 'static {
        self.proves.fetch_add(1, Ordering::SeqCst);
        let fresh = self.fresh.lock().expect("fresh receipt mutex").clone();
        async move { Ok(fresh) }
    }

    fn journal_bytes(receipt: &Vec<u8>) -> Vec<u8> {
        receipt.clone()
    }

    fn batch_image_id(&self) -> &[u8; 32] {
        &BATCH_IMAGE_ID
    }
}

/// Processor that writes every resource a transaction touches, so each batch settles its own
/// state root, and publishes the transaction's artifact immediately.
#[derive(Clone)]
struct WritingProcessor;

impl Processor<ParkingStore> for WritingProcessor {
    fn process_transaction(
        &self,
        ctx: &mut TransactionContext<ParkingStore, Self>,
    ) -> Result<(), Self::Error> {
        for resource in ctx.resources_mut() {
            resource.set_data(vec![1]);
        }
        ctx.scheduled_tx().publish_artifact(Some(Vec::new()));
        Ok(())
    }

    fn tx_image_id(&self) -> [u8; 32] {
        TX_IMAGE_ID
    }

    fn batch_image_id(&self) -> [u8; 32] {
        BATCH_IMAGE_ID
    }

    type Transaction = usize;
    type TransactionArtifact = Vec<u8>;
    type BatchArtifact = Vec<u8>;
    type AggregatorArtifact = Vec<u8>;
    type BatchMetadata = ChainBlockMetadata;
    type Error = ();
}

/// Chain-block metadata whose only distinguishing fields are the block hash and its parent id.
fn block(hash: u8, parent_id: u64) -> ChainBlockMetadata {
    ChainBlockMetadata { hash: Hash::from_bytes([hash; 32]), parent_id, ..Default::default() }
}

/// One lane transaction, so its batch is non-empty and reaches the receipt lookup.
fn lane_tx(resource: usize) -> SchedulerTransaction<usize> {
    SchedulerTransaction::new(0, vec![AccessMetadata::write(ResourceId::for_test(resource))], 0)
}

/// Reads the receipt the cache holds at `batch`'s coordinate.
fn cached_receipt(batch: &ScheduledBatch<ParkingStore, WritingProcessor>) -> Option<Vec<u8>> {
    let runtime = Builder::new_current_thread().build().expect("runtime");
    runtime.block_on(batch.read_batch_receipt().resolve())
}

/// A committed anchor and a committed victim batch above it, with a batch prover whose next
/// tree read can be parked.
struct Fixture {
    /// Keeps the store's directory alive.
    _temp_dir: TempDir,
    /// The store, for reading roots.
    storage: ParkingStore,
    /// Scheduler the batches were committed through.
    scheduler: Scheduler<ParkingStore, WritingProcessor>,
    /// The batch the rollback cancels while the worker collects its inputs.
    victim: ScheduledBatch<ParkingStore, WritingProcessor>,
    /// Proves the backend has run.
    proves: Arc<AtomicUsize>,
    /// The receipt a fresh proof of the victim yields: it enters at the anchor's root and
    /// settles the victim's.
    fresh: Vec<u8>,
    /// The batch prover under test.
    prover: BatchProver<ParkingStore, WritingProcessor>,
}

impl Fixture {
    /// Commits the anchor and the victim and starts the prover.
    fn new() -> Self {
        let temp_dir = TempDir::new().expect("failed to create temp dir");
        let storage =
            ParkingStore { inner: RocksDbStore::open(temp_dir.path()), park: Arc::default() };
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(WritingProcessor),
            StorageConfig::default().with_store(storage.clone()),
        );
        let anchor = scheduler.schedule(block(1, 0), vec![lane_tx(1)]);
        anchor.wait_committed_blocking();
        let victim = scheduler.schedule(block(2, 1), vec![lane_tx(2)]);
        victim.wait_committed_blocking();

        let fresh = receipt(storage.root(1), storage.root(2));
        let backend = CountingBackend::default();
        *backend.fresh.lock().expect("fresh receipt mutex") = fresh.clone();
        let proves = backend.proves.clone();
        let prover = BatchProver::new(
            backend,
            storage.clone(),
            BatchProverConfig {
                lane_key: Hash::default(),
                covenant_id: Hash::default(),
                deposit_spk_hash: [0u8; 32],
            },
        );
        Self { _temp_dir: temp_dir, storage, scheduler, victim, proves, fresh, prover }
    }

    /// Hands the victim to the worker, rolls execution back to `target` while the worker's
    /// first tree read for it is parked, and lets the read go on.
    fn roll_back_during_the_first_tree_read(&mut self, target: u64) {
        self.roll_back_during_tree_read(1, target);
    }

    /// The same for the worker's second tree read. With a stale receipt cached, the first read
    /// is the entry check that finds it stale and the second is the proof.
    fn roll_back_during_the_second_tree_read(&mut self, target: u64) {
        self.roll_back_during_tree_read(2, target);
    }

    /// Hands the victim to the worker, rolls execution back to `target` while the worker's
    /// `nth` tree read for it is parked, and lets the read go on.
    fn roll_back_during_tree_read(&mut self, nth: usize, target: u64) {
        self.storage.park.until_parked(nth, || self.prover.submit(&self.victim));
        self.scheduler.rollback_to(target).expect("rollback");
        assert!(self.victim.canceled(), "the rollback cancels the victim");
        self.storage.park.release();
    }

    /// Hands `batch` to the worker and waits until its artifact is published, which the worker
    /// only gets to once it is done with the victim.
    fn wait_served(&self, batch: &ScheduledBatch<ParkingStore, WritingProcessor>) {
        self.prover.submit(batch);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !batch.artifact_published() {
            assert!(Instant::now() < deadline, "the worker never got past the canceled victim");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Waits until the receipt cached at the victim's coordinate is the fresh proof's.
    fn wait_fresh_receipt_cached(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while cached_receipt(&self.victim) != Some(self.fresh.clone()) {
            assert!(Instant::now() < deadline, "the canceled batch's proof never landed");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Schedules a batch on top of genesis whose receipt is already cached, so serving it takes
    /// no proof, and waits until the worker has served it.
    fn serve_a_cached_batch_from_genesis(&mut self) {
        let probe = self.scheduler.schedule(block(3, 0), vec![lane_tx(3)]);
        probe.wait_committed_blocking();
        let index = probe.checkpoint().index();
        probe
            .write_batch_receipt(receipt(self.storage.root(index - 1), self.storage.root(index)))
            .wait_blocking();
        self.wait_served(&probe);
    }

    /// Shuts the prover and the scheduler down.
    fn shutdown(self) {
        self.prover.shutdown();
        self.scheduler.shutdown();
    }
}

/// A rollback landing while a cached receipt's entry is checked makes a receipt that proves the
/// batch look stale: the root it must enter at is read from the rolled-back chain. The worker
/// must not prove the canceled batch over it, and the cached receipt stays as it is.
#[test]
fn rollback_during_the_entry_check_does_not_prove_the_canceled_batch() {
    let mut fixture = Fixture::new();
    let valid = fixture.fresh.clone();
    fixture.victim.write_batch_receipt(valid.clone()).wait_blocking();

    fixture.roll_back_during_the_first_tree_read(0);
    fixture.serve_a_cached_batch_from_genesis();

    assert_eq!(fixture.proves.load(Ordering::SeqCst), 0, "a canceled batch is not proved");
    assert_eq!(cached_receipt(&fixture.victim), Some(valid), "its cached receipt is untouched");
    fixture.shutdown();
}

/// A rollback landing while the SMT proof of an uncached batch is collected roots the proof in
/// another lineage than the batch's transaction receipts. The worker must not prove the
/// canceled batch from it, and nothing is cached for it.
#[test]
fn rollback_during_the_proof_read_does_not_prove_the_canceled_batch() {
    let mut fixture = Fixture::new();

    fixture.roll_back_during_the_first_tree_read(0);
    fixture.serve_a_cached_batch_from_genesis();

    assert_eq!(fixture.proves.load(Ordering::SeqCst), 0, "a canceled batch is not proved");
    assert_eq!(cached_receipt(&fixture.victim), None, "nothing is cached for it");
    fixture.shutdown();
}

/// A stale cached receipt takes the worker past the entry check into the proof read. A rollback
/// landing there roots the proof in another lineage, as it does for an uncached batch. The
/// worker must not prove the canceled batch from it, and the stale receipt stays as it is.
#[test]
fn rollback_during_the_proof_read_does_not_replace_a_stale_receipt() {
    let mut fixture = Fixture::new();
    let stale = receipt([0xEE; 32], fixture.storage.root(2));
    fixture.victim.write_batch_receipt(stale.clone()).wait_blocking();

    fixture.roll_back_during_the_second_tree_read(0);
    fixture.serve_a_cached_batch_from_genesis();

    assert_eq!(fixture.proves.load(Ordering::SeqCst), 0, "a canceled batch is not proved");
    assert_eq!(cached_receipt(&fixture.victim), Some(stale), "its cached receipt is untouched");
    fixture.shutdown();
}

/// A rollback to exactly the version the batch reads at (a re-root to the boundary when the
/// batch has the next id) cancels the batch without touching what it read: the proof it
/// collected is its own. The worker must still prove it, so the block's return under the same
/// batch id finds the receipt cached and takes no second proof.
#[test]
fn rollback_to_the_entry_itself_still_proves_the_batch_for_its_return() {
    let mut fixture = Fixture::new();

    // The canceled batch is still proved, and its receipt lands in the cache.
    fixture.roll_back_during_the_first_tree_read(1);
    fixture.wait_fresh_receipt_cached();
    assert_eq!(fixture.proves.load(Ordering::SeqCst), 1);

    // The block returns under its batch id and is served from that receipt.
    let returned = fixture.scheduler.schedule(block(2, 1), vec![lane_tx(2)]);
    assert_eq!(returned.checkpoint().index(), 2, "the block returns under its batch id");
    fixture.wait_served(&returned);
    assert_eq!(*returned.artifact(), fixture.fresh, "the return publishes the cached receipt");
    assert_eq!(fixture.proves.load(Ordering::SeqCst), 1, "the return takes no second proof");
    fixture.shutdown();
}

/// The same with a stale receipt cached: a rollback to exactly the version the batch reads at,
/// landing while the receipt's entry is checked, leaves the verdict standing. The canceled
/// batch is still proved afresh, and the fresh receipt replaces the stale one for the block's
/// return.
#[test]
fn rollback_to_the_entry_itself_still_replaces_a_stale_receipt() {
    let mut fixture = Fixture::new();
    let stale = receipt([0xEE; 32], fixture.storage.root(2));
    fixture.victim.write_batch_receipt(stale).wait_blocking();

    fixture.roll_back_during_the_first_tree_read(1);
    fixture.wait_fresh_receipt_cached();
    assert_eq!(fixture.proves.load(Ordering::SeqCst), 1, "the canceled batch is proved once");
    fixture.shutdown();
}

/// With orphaned ids between a batch and its parent, a rollback to the parent can change what
/// the batch reads: a sibling fork returns under one of those ids. The batch reads at the
/// version right below its own id, so a rollback to its parent reaches below that, and the
/// worker must drop the batch although its parent still stands.
#[test]
fn rollback_to_the_parent_across_an_id_hole_does_not_prove_the_canceled_batch() {
    let mut fixture = Fixture::new();

    // The fixture's second batch becomes a sibling fork: it is rolled back and the victim is
    // scheduled on the anchor instead, which leaves id 2 as an orphaned hole below its id 3.
    fixture.scheduler.rollback_to(1).expect("rollback to the anchor");
    let victim = fixture.scheduler.schedule(block(3, 1), vec![lane_tx(3)]);
    victim.wait_committed_blocking();
    assert_eq!(victim.checkpoint().index(), 3, "batch ids are never reused");

    // While the victim's proof read is parked, execution rolls back to the anchor and the
    // sibling returns under id 2, the version the victim reads at.
    fixture.storage.park.until_parked(1, || fixture.prover.submit(&victim));
    fixture.scheduler.rollback_to(1).expect("rollback to the anchor");
    assert!(victim.canceled(), "the rollback cancels the victim");
    let sibling = fixture.scheduler.schedule(block(2, 1), vec![lane_tx(2)]);
    sibling.wait_committed_blocking();
    assert_eq!(sibling.checkpoint().index(), 2, "the sibling returns under its batch id");
    fixture.storage.park.release();

    // The sibling's receipt is cached, so serving it behind the victim takes no proof.
    sibling.write_batch_receipt(fixture.fresh.clone()).wait_blocking();
    fixture.wait_served(&sibling);
    assert_eq!(fixture.proves.load(Ordering::SeqCst), 0, "a canceled batch is not proved");
    assert_eq!(cached_receipt(&victim), None, "nothing is cached for it");
    fixture.shutdown();
}
