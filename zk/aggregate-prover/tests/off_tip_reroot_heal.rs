//! Pins the heal of an off-tip committed gap across the re-root it requests, with the real batch
//! prover in the loop.
//!
//! A committed batch above the settled boundary carries a cached receipt that enters at a lane
//! tip the covenant never took, so the committed-gap pass holds the re-cover and requests a
//! re-root. The re-root rolls execution back to the boundary and the bridge re-feeds the block,
//! which returns under its original batch id and block hash: the coordinate the batch prover
//! looks its receipt up by. Reusing that cached receipt hands the stale one straight back, the
//! gap pass reads it again, and the range never chains from the tip. The batch prover must
//! prove the re-fed batch afresh, so the retry finds a receipt entering at the tip's exit and
//! composes, also when a further re-root cancels the batch while its proof is in flight.
//!
//! The same holds one level up. If the off-tip range was bundled before, the aggregate composed
//! from the stale receipt is cached at the coordinate the re-cover composes, and the aggregate
//! prover must prove the bundle afresh from the re-proved batch receipt.

// The backend traits return `impl Future + 'static`, which an `async fn` cannot satisfy: its future
// borrows `&self`.
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
use kaspa_rpc_core::GetSeqCommitLaneProofResponse;
use tempfile::TempDir;
use tokio::{
    runtime::Builder,
    sync::{mpsc, watch},
};
use vprogs_core_atomics::AsyncQueue;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId, SchedulerTransaction};
use vprogs_l1_types::{ChainBlockMetadata, SettlementInfo};
use vprogs_scheduling_scheduler::{
    ExecutionConfig, Processor, ScheduledBatch, Scheduler, TransactionContext,
};
use vprogs_state_proof_receipt::AggregatorKey;
use vprogs_state_settlement_journal::StoreJournal;
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::Store;
use vprogs_zk_abi::{
    batch_aggregator::{StateTransition, StateTransitionArgs},
    batch_processor::{BatchTransition, BatchTransitionArgs},
};
use vprogs_zk_aggregate_prover::{
    AggregateProver, AggregateProverConfig, BundleBlocks, ScheduledBundle, SettlementArtifact,
};
use vprogs_zk_batch_prover::{
    BatchProver, BatchProverConfig, LaneProofError, LaneProofRequest, LaneProofSource,
};
use zerocopy::FromBytes;

/// Transaction-guest image id; the processor publishes tx artifacts directly, so it keys nothing.
const TX_IMAGE_ID: [u8; 32] = [0u8; 32];
/// Batch-guest image id, keying a per-batch receipt in the proof-receipt store.
const BATCH_IMAGE_ID: [u8; 32] = [1u8; 32];
/// Aggregator-guest image id, keying a bundle's settlement receipt.
const AGGREGATOR_IMAGE_ID: [u8; 32] = [2u8; 32];

/// `seq_commit` every block's metadata and the synthetic settlement journal carry.
fn seq_commit() -> Hash {
    Hash::from_bytes([0x33; 32])
}

/// Block-hash helper keyed to the single byte every test block is built from.
fn block_hash(byte: u8) -> Hash {
    Hash::from_bytes([byte; 32])
}

/// Chain-block metadata entering at `prev_lane_tip` and leaving the lane at the block's own
/// hash.
fn block(hash: u8, parent_id: u64, prev_lane_tip: Hash) -> ChainBlockMetadata {
    ChainBlockMetadata {
        hash: block_hash(hash),
        parent_id,
        seq_commit: seq_commit(),
        prev_lane_tip,
        lane_tip: block_hash(hash),
        ..Default::default()
    }
}

/// One end of a batch transition: the state root and lane tip at it.
#[derive(Clone, Copy)]
struct Pins {
    /// State root at this end.
    state: [u8; 32],
    /// Lane tip at this end.
    lane_tip: Hash,
}

/// A batch receipt whose journal runs from `entry` to `exit`. The backend's `journal_bytes` is
/// the identity, so the receipt is its own journal.
fn batch_receipt(entry: Pins, exit: Pins) -> Vec<u8> {
    let mut journal = Vec::new();
    BatchTransition::encode(
        &mut journal,
        BatchTransitionArgs {
            prev_state: &entry.state,
            prev_lane_tip: &entry.lane_tip,
            prev_lane_blue_score: 0,
            new_state: &exit.state,
            new_lane_tip: &exit.lane_tip,
            new_lane_blue_score: 0,
            lane_key: &Hash::default(),
            covenant_id: &[0u8; 32],
            tx_image_id: &TX_IMAGE_ID,
            deposit_spk_hash: &[0u8; 32],
            lane_expired: false,
            exits: b"",
        },
    );
    journal
}

/// The settlement journal an aggregate over `batch_receipts` commits: it enters where the first
/// batch does and exits where the last one does, as the aggregator guest composes it, with a
/// `new_seq_commit` matching [`seq_commit`].
fn settlement_journal(batch_receipts: &[Vec<u8>]) -> Vec<u8> {
    let first = batch_receipts.first().expect("a bundle composes at least one batch");
    let last = batch_receipts.last().expect("a bundle composes at least one batch");
    let first = BatchTransition::ref_from_bytes(first).expect("batch journal");
    let last = BatchTransition::ref_from_bytes(last).expect("batch journal");
    let mut buf = Vec::new();
    StateTransition::encode(
        &mut buf,
        StateTransitionArgs {
            prev_state: &first.prev_state,
            prev_lane_tip: &first.prev_lane_tip,
            new_state: &last.new_state,
            new_lane_tip: &last.new_lane_tip,
            new_seq_commit: &seq_commit(),
            covenant_id: &[0u8; 32],
            tx_image_id: &TX_IMAGE_ID,
            batch_image_id: &BATCH_IMAGE_ID,
            permission_spk_hash: &[0u8; 32],
            deposit_spk_hash: &[0u8; 32],
            lane_key: &Hash::default(),
        },
    );
    buf
}

/// Backend standing in for all three guests. The batch prove hands out the receipt the test
/// scripted, counts every call and can be parked mid-proof; the aggregator prove composes the
/// settlement journal from the batch receipts it is given.
#[derive(Clone, Default)]
struct ScriptedBackend {
    /// Receipt a batch prove returns.
    fresh: Arc<Mutex<Vec<u8>>>,
    /// Batch proves started so far.
    proves: Arc<AtomicUsize>,
    /// While set, a batch prove stays in flight.
    parked: Arc<AtomicBool>,
}

impl vprogs_zk_transaction_prover::Backend for ScriptedBackend {
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

impl vprogs_zk_batch_prover::Backend for ScriptedBackend {
    fn prove_batch(
        &self,
        _inputs: &[u8],
        _receipts: Vec<Vec<u8>>,
    ) -> impl Future<Output = Result<Vec<u8>, String>> + Send + 'static {
        self.proves.fetch_add(1, Ordering::SeqCst);
        let (fresh, parked) = (self.fresh.clone(), self.parked.clone());
        async move {
            while parked.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let receipt = fresh.lock().expect("fresh receipt mutex").clone();
            Ok(receipt)
        }
    }

    fn journal_bytes(receipt: &Vec<u8>) -> Vec<u8> {
        receipt.clone()
    }

    fn batch_image_id(&self) -> &[u8; 32] {
        &BATCH_IMAGE_ID
    }
}

impl vprogs_zk_aggregate_prover::Backend for ScriptedBackend {
    fn prove_aggregator(
        &self,
        _inputs: &[u8],
        batch_receipts: Vec<Vec<u8>>,
    ) -> impl Future<Output = Result<Vec<u8>, String>> + Send + 'static {
        async move { Ok(settlement_journal(&batch_receipts)) }
    }

    fn aggregator_image_id(&self) -> &[u8; 32] {
        &AGGREGATOR_IMAGE_ID
    }
}

/// Lane source serving every fetch.
struct ServeLaneProofs;

impl LaneProofSource for ServeLaneProofs {
    async fn fetch_lane_proof(
        &self,
        _req: LaneProofRequest,
    ) -> Result<GetSeqCommitLaneProofResponse, LaneProofError> {
        Ok(GetSeqCommitLaneProofResponse {
            smt_proof: Vec::new(),
            lane: None,
            payload_and_ctx_digest: Hash::default(),
            parent_seq_commit: Hash::default(),
            inactivity_shortcut: Hash::default(),
        })
    }
}

/// Processor that writes every resource a transaction touches, so each batch settles its own
/// state root, and publishes the transaction's artifact immediately, so the batch prover's wait
/// on the batch's tx artifacts resolves.
#[derive(Clone)]
struct WritingProcessor;

impl Processor<RocksDbStore> for WritingProcessor {
    fn process_transaction(
        &self,
        ctx: &mut TransactionContext<RocksDbStore, Self>,
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

/// One lane transaction, so its batch is non-empty.
fn lane_tx(resource: usize) -> SchedulerTransaction<usize> {
    SchedulerTransaction::new(0, vec![AccessMetadata::write(ResourceId::for_test(resource))], 0)
}

/// The state root `store` holds at `version`.
fn root_at<S: Store>(store: &S, version: u64) -> [u8; 32] {
    store.root(version)
}

/// Pops the next bundle the worker emits, or `None` once `timeout` elapses.
fn next_bundle(
    queue: &AsyncQueue<ScheduledBundle<SettlementArtifact<Vec<u8>>>>,
    timeout: Duration,
) -> Option<ScheduledBundle<SettlementArtifact<Vec<u8>>>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(bundle) = queue.pop() {
            return Some(bundle);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// A node whose committed batch 2 sits above the settled boundary with a cached receipt entering
/// off the covenant tip, booted up to the re-root request of the startup gap pass.
struct Fixture {
    /// Keeps the store's directory alive.
    _temp_dir: TempDir,
    /// Scheduler the blocks are fed through.
    scheduler: Scheduler<RocksDbStore, WritingProcessor>,
    /// The batch prover serving the re-fed batches.
    batch_prover: BatchProver<RocksDbStore, WritingProcessor>,
    /// The aggregate prover whose gap pass re-covers the range.
    prover: AggregateProver<RocksDbStore, WritingProcessor>,
    /// Bundles the aggregate prover emitted.
    queue: AsyncQueue<ScheduledBundle<SettlementArtifact<Vec<u8>>>>,
    /// Re-root requests the aggregate prover sent.
    re_root_rx: mpsc::UnboundedReceiver<SettlementInfo>,
    /// Handles on the backend both provers share.
    backend: ScriptedBackend,
    /// The pins the covenant tip exits at: the boundary batch's state root and lane tip.
    at_boundary: Pins,
    /// The receipt cached at batch 2's coordinate, entering off the tip.
    stale: Vec<u8>,
    /// The receipt a fresh proof of the re-fed batch 2 yields: it enters at the tip's exit.
    reproved: Vec<u8>,
}

impl Fixture {
    /// Commits the boundary batch and the off-tip batch above it, starts both provers and
    /// consumes the re-root request the startup gap pass sends.
    fn new() -> Self {
        let temp_dir = TempDir::new().expect("failed to create temp dir");
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let journal = StoreJournal::new(storage.clone());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(WritingProcessor),
            StorageConfig::default().with_store(storage.clone()),
        );

        // The covenant's boundary batch, committed with its receipt.
        let boundary = scheduler.schedule(block(1, 0, Hash::default()), vec![lane_tx(1)]);
        boundary.wait_committed_blocking();
        let at_boundary = Pins { state: root_at(&storage, 1), lane_tip: block_hash(1) };
        boundary
            .write_batch_receipt(batch_receipt(
                Pins { state: root_at(&storage, 0), lane_tip: Hash::default() },
                at_boundary,
            ))
            .wait_blocking();

        // The batch above it was first fed entering at a lane tip the covenant never took, and
        // its receipt was proved and cached that way: committed, never journaled, off the tip.
        let off_tip = Pins { state: at_boundary.state, lane_tip: block_hash(0x99) };
        let forked = scheduler.schedule(block(2, 1, off_tip.lane_tip), vec![lane_tx(2)]);
        forked.wait_committed_blocking();
        let exit = Pins { state: root_at(&storage, 2), lane_tip: block_hash(2) };
        let stale = batch_receipt(off_tip, exit);
        forked.write_batch_receipt(stale.clone()).wait_blocking();

        // Re-executed from the boundary the block settles the same root, and a fresh proof of
        // it enters at the boundary's pins.
        let reproved = batch_receipt(at_boundary, exit);
        let backend = ScriptedBackend::default();
        *backend.fresh.lock().expect("fresh receipt mutex") = reproved.clone();
        let batch_prover = BatchProver::new(
            backend.clone(),
            storage.clone(),
            BatchProverConfig {
                lane_key: Hash::default(),
                covenant_id: Hash::default(),
                deposit_spk_hash: [0u8; 32],
            },
        );
        let queue: AsyncQueue<ScheduledBundle<SettlementArtifact<Vec<u8>>>> = AsyncQueue::new();
        let (settlement_tx, settlement_rx) = watch::channel::<Option<SettlementInfo>>(None);
        let (re_root_tx, mut re_root_rx) = mpsc::unbounded_channel();
        let prover = AggregateProver::new(
            backend.clone(),
            scheduler.state().receipt_store(),
            Some(journal),
            AggregateProverConfig {
                lane_key: Hash::default(),
                covenant_id: None,
                lane_source: ServeLaneProofs,
                settlement_queue: Some(queue.clone()),
                settlement: Some(settlement_rx),
                re_root: Some(re_root_tx),
                bundle_size: 1..=1,
                exits: None,
                tip_daa: None,
                follow_depth: 0,
            },
        );

        // The covenant tip settled through the boundary batch and exits at its pins. The
        // startup gap pass finds batch 2's receipt entering elsewhere, holds the re-cover and
        // requests the re-root.
        settlement_tx.send_replace(Some(SettlementInfo {
            block_prove_to: block_hash(1),
            new_state: at_boundary.state,
            new_lane_tip: at_boundary.lane_tip,
            ..Default::default()
        }));
        prover.wait_booted();
        re_root_rx.try_recv().expect("the off-tip range must request a re-root");
        assert!(next_bundle(&queue, Duration::from_millis(200)).is_none(), "nothing composes yet");

        Self {
            _temp_dir: temp_dir,
            scheduler,
            batch_prover,
            prover,
            queue,
            re_root_rx,
            backend,
            at_boundary,
            stale,
            reproved,
        }
    }

    /// Honors the pending re-root request the way the runner does: execution rolls back to the
    /// boundary and both provers hear it. The re-root re-arms the request, so the retry it
    /// wakes still reads the stale receipt and asks once more; waiting for that request pins
    /// the retry before anything else moves.
    fn honor_reroot(&mut self) {
        self.scheduler.reroot_to(1).expect("re-root to the boundary");
        self.batch_prover.rollback(1);
        self.prover.reroot(1);
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.re_root_rx.try_recv().is_err() {
            assert!(Instant::now() < deadline, "the retry after the re-root never ran");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Re-feeds block 2 the way the bridge does after a re-root, now threaded from the
    /// boundary's lane tip, and hands it to both provers. It returns under its original batch
    /// id and block hash, so whatever receipt is cached at its coordinate is a hit.
    fn refeed(&mut self) -> ScheduledBatch<RocksDbStore, WritingProcessor> {
        let refed =
            self.scheduler.schedule(block(2, 1, self.at_boundary.lane_tip), vec![lane_tx(2)]);
        assert_eq!(refed.checkpoint().index(), 2, "the re-fed block returns under its original id");
        self.batch_prover.submit(&refed);
        self.prover.submit(&refed);
        refed
    }

    /// Reads the receipt cached at batch 2's coordinate.
    fn cached(&self, batch: &ScheduledBatch<RocksDbStore, WritingProcessor>) -> Option<Vec<u8>> {
        let runtime = Builder::new_current_thread().build().expect("runtime");
        runtime.block_on(batch.read_batch_receipt().resolve())
    }

    /// Writes `aggregate` to the cache at the coordinate of a bundle over batch 2 alone, the
    /// range the gap retry re-covers.
    fn cache_aggregate(&self, aggregate: Vec<u8>) {
        let receipt_store = self.scheduler.state().receipt_store();
        receipt_store.write_agg_receipt(Self::re_cover_key(), aggregate).wait_blocking();
    }

    /// Reads the aggregate cached at the coordinate of a bundle over batch 2 alone.
    fn cached_aggregate(&self) -> Option<Vec<u8>> {
        let receipt_store = self.scheduler.state().receipt_store();
        let runtime = Builder::new_current_thread().build().expect("runtime");
        runtime.block_on(receipt_store.read_agg_receipt(Self::re_cover_key()).resolve())
    }

    /// The aggregate cache key of a bundle over batch 2 alone.
    fn re_cover_key() -> AggregatorKey {
        let blocks = BundleBlocks { from_block: block_hash(2), block_prove_to: block_hash(2) };
        ScheduledBundle::<()>::new(1, 2, blocks)
            .agg_key(AGGREGATOR_IMAGE_ID, seq_commit().as_bytes())
    }

    /// Waits for the gap retry to re-cover batch 2 and checks that the bundle it emits chains
    /// from the covenant tip.
    fn expect_re_cover(&self) {
        let covered = next_bundle(&self.queue, Duration::from_secs(10))
            .expect("the off-tip range must be re-covered once the batch is proved afresh");
        covered.wait_artifact_published_blocking();
        assert_eq!(covered.checkpoint_index(), 2, "the re-cover starts above the boundary");
        assert_eq!(covered.block_prove_to(), block_hash(2));
        let artifact = covered.artifact().expect("the re-cover settles a state transition");
        assert_eq!(artifact.prev_state, self.at_boundary.state, "it enters at the tip's state");
        assert_eq!(artifact.prev_lane_tip, self.at_boundary.lane_tip, "and at the tip's lane tip");
    }

    /// Shuts both provers and the scheduler down.
    fn shutdown(self) {
        self.prover.shutdown();
        self.batch_prover.shutdown();
        self.scheduler.shutdown();
    }
}

/// Tests that a committed gap whose cached receipt enters off the covenant tip heals through
/// the re-root it requests.
///
/// The re-fed batch returns under the coordinate its stale receipt is cached at. The batch
/// prover must prove it afresh instead of handing the stale receipt back, so the gap retry
/// reads a receipt that enters at the tip's exit and re-covers the range. With the stale
/// receipt reused, the retry's chaining check fails forever and the range never settles.
#[test]
fn off_tip_gap_heals_once_the_reroot_reproves_the_batch() {
    let mut fixture = Fixture::new();
    fixture.honor_reroot();

    // The bridge re-feeds block 2. The batch prover proves it afresh, and the gap retry
    // re-covers the range from the receipt that now enters at the tip's exit.
    let refed = fixture.refeed();
    fixture.expect_re_cover();
    refed.wait_artifact_published_blocking();
    assert_ne!(fixture.reproved, fixture.stale);
    assert_eq!(*refed.artifact(), fixture.reproved, "the re-fed batch carries the fresh receipt");
    assert_eq!(fixture.backend.proves.load(Ordering::SeqCst), 1, "the batch is proved once");
    assert_eq!(
        fixture.cached(&refed),
        Some(fixture.reproved.clone()),
        "the fresh receipt replaces the stale one in the cache",
    );

    fixture.shutdown();
}

/// Tests that the heal survives a further re-root landing while the re-fed batch is still being
/// proved.
///
/// The gap retry keeps reading the stale receipt until the fresh one is durable, so it requests
/// the re-root again, and honoring that request cancels the batch mid-proof. The proof still
/// completes and its receipt still replaces the stale one, so the block's next return is served
/// from the cache without proving, and the retry re-covers the range.
#[test]
fn off_tip_gap_heals_when_a_further_reroot_cancels_the_reproving_batch() {
    let mut fixture = Fixture::new();
    fixture.honor_reroot();

    // The bridge re-feeds block 2 and its fresh proof stays in flight.
    fixture.backend.parked.store(true, Ordering::SeqCst);
    let canceled = fixture.refeed();
    canceled.wait_committed_blocking();
    let deadline = Instant::now() + Duration::from_secs(10);
    while fixture.backend.proves.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "the re-fed batch never reached its proof");
        thread::sleep(Duration::from_millis(5));
    }

    // The runner honors the request the retry re-armed: the batch is canceled mid-proof, and
    // the retry that follows still reads the stale receipt.
    fixture.honor_reroot();
    assert!(canceled.canceled(), "the further re-root cancels the re-fed batch");
    assert_eq!(fixture.cached(&canceled), Some(fixture.stale.clone()));

    // The proof completes for the canceled batch and its receipt replaces the stale one.
    fixture.backend.parked.store(false, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(10);
    while fixture.cached(&canceled) != Some(fixture.reproved.clone()) {
        assert!(Instant::now() < deadline, "the canceled batch's fresh receipt never landed");
        thread::sleep(Duration::from_millis(5));
    }

    // The block returns once more. Its cached receipt now enters where the batch does, so it
    // is reused without proving, and the gap retry re-covers the range from it.
    let refed = fixture.refeed();
    fixture.expect_re_cover();
    refed.wait_artifact_published_blocking();
    assert_eq!(*refed.artifact(), fixture.reproved, "the returning batch reuses the receipt");
    assert_eq!(fixture.backend.proves.load(Ordering::SeqCst), 1, "the batch is proved once");

    fixture.shutdown();
}

/// Tests that the heal gets past the aggregate proved over the stale batch receipt.
///
/// The off-tip range was bundled once before: the aggregate composed from the stale batch
/// receipt is cached at the range's coordinate, and its journal entry is gone (the resume pass
/// drops an entry that chains from a base the covenant never took). The re-cover composes the
/// same range, so that aggregate is a hit. The aggregate prover must prove the bundle afresh
/// from the re-proved batch receipt. Served again, the stale aggregate enters off the tip, the
/// settler skips it, and the range never settles.
#[test]
fn off_tip_gap_heals_over_the_aggregate_of_the_stale_receipt() {
    let mut fixture = Fixture::new();

    // Nothing reads the aggregate cache before the retry composes, so writing the dead bundle's
    // aggregate now is the same as finding it there at startup.
    let stale_aggregate = settlement_journal(&[fixture.stale.clone()]);
    fixture.cache_aggregate(stale_aggregate.clone());
    fixture.honor_reroot();

    // The batch is proved afresh, and the re-cover is composed from its fresh receipt.
    let refed = fixture.refeed();
    fixture.expect_re_cover();
    refed.wait_artifact_published_blocking();
    let fresh_aggregate = settlement_journal(&[fixture.reproved.clone()]);
    assert_ne!(fresh_aggregate, stale_aggregate);
    assert_eq!(
        fixture.cached_aggregate(),
        Some(fresh_aggregate),
        "the fresh aggregate replaces the stale one in the cache",
    );

    fixture.shutdown();
}
