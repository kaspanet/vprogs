//! Pins when the aggregate prover reuses a cached aggregate receipt.
//!
//! The cache is keyed by the bundle's range: its first checkpoint and block, the aggregator image
//! and the end block's `seq_commit`. That does not name the batch receipts the aggregate was
//! composed from. Once a re-root has re-proved a range from another entry, the aggregate proved
//! over the replaced receipts is still a hit for the range. Served as it was, the bundle enters
//! where the covenant never was: the settler skips it, and the fresh batch receipts are never
//! composed.
//!
//! The worker must reuse a cached aggregate only while it enters where the bundle's first batch
//! receipt does and exits where its last one does, which is what the aggregator guest commits
//! for a bundle over them, and prove the bundle afresh otherwise.

// The backend traits return `impl Future + 'static`, which an `async fn` cannot satisfy: its future
// borrows `&self`.
#![allow(clippy::manual_async_fn)]

use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use kaspa_hashes::Hash;
use kaspa_rpc_core::GetSeqCommitLaneProofResponse;
use tempfile::TempDir;
use tokio::{runtime::Builder, time::timeout};
use vprogs_core_atomics::AsyncQueue;
use vprogs_core_codec::Reader;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId, SchedulerTransaction};
use vprogs_l1_types::ChainBlockMetadata;
use vprogs_scheduling_scheduler::{ExecutionConfig, Processor, Scheduler, TransactionContext};
use vprogs_state_proof_receipt::AggregatorKey;
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_abi::{
    batch_aggregator::{
        Inputs as AggregatorInputs, StateTransition, StateTransitionArgs, Verifier,
    },
    batch_processor::{BatchTransition, BatchTransitionArgs},
    withdrawal::{ExitAccumulator, StandardSpk},
};
use vprogs_zk_aggregate_prover::{
    AggregateProver, AggregateProverConfig, BundleBlocks, ScheduledBundle, SettlementArtifact,
};
use vprogs_zk_batch_prover::{LaneProofError, LaneProofRequest, LaneProofSource};
use zerocopy::FromBytes;

/// Transaction-guest image id. Nothing is proved at that level, so it keys nothing.
const TX_IMAGE_ID: [u8; 32] = [0u8; 32];
/// Batch-guest image id, keying a per-batch receipt in the proof-receipt store.
const BATCH_IMAGE_ID: [u8; 32] = [1u8; 32];
/// Aggregator-guest image id, keying a bundle's settlement receipt.
const AGGREGATOR_IMAGE_ID: [u8; 32] = [2u8; 32];

/// Exit-output hash only the cached aggregate of the reuse test carries. A fresh aggregate has
/// none, so the hash on a bundle's artifact tells the cached receipt from a fresh one.
const CACHED_MARK: [u8; 32] = [0x77; 32];

/// `seq_commit` every block's metadata and every aggregate journal carry: the one the aggregator
/// guest derives for the bundle's final block, which the worker checks a bundle's journal
/// against.
fn seq_commit() -> Hash {
    let journal = guest_aggregate();
    (&mut &journal[..])
        .array_as::<StateTransition>("state_transition")
        .expect("aggregate journal")
        .new_seq_commit
}

/// Block-hash helper keyed to the single byte every test block is built from.
fn block_hash(byte: u8) -> Hash {
    Hash::from_bytes([byte; 32])
}

/// Chain-block metadata whose only distinguishing fields are the block hash and its parent id.
fn block(hash: u8, parent_id: u64) -> ChainBlockMetadata {
    ChainBlockMetadata {
        hash: block_hash(hash),
        parent_id,
        seq_commit: seq_commit(),
        ..Default::default()
    }
}

/// One end of a transition: the state root and lane tip at it.
#[derive(Clone, Copy)]
struct Pins {
    /// State root at this end.
    state: [u8; 32],
    /// Lane tip at this end.
    lane_tip: Hash,
}

/// Pins built from `byte`. The state root and the lane tip differ, so a compare against the
/// wrong one of the two cannot pass.
fn pins(byte: u8) -> Pins {
    Pins { state: [byte; 32], lane_tip: Hash::from_bytes([byte + 1; 32]) }
}

/// Where the bundle's first batch enters.
fn entry() -> Pins {
    pins(0xA0)
}

/// Where the first batch exits and the last one enters.
fn middle() -> Pins {
    pins(0xB0)
}

/// Where the bundle's last batch exits.
fn exit() -> Pins {
    pins(0xC0)
}

/// Pins no batch of the bundle carries.
fn elsewhere() -> Pins {
    pins(0xE0)
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

/// An aggregate receipt whose journal runs from `entry` to `exit` and carries
/// `permission_spk_hash`.
fn aggregate(entry: Pins, exit: Pins, permission_spk_hash: [u8; 32]) -> Vec<u8> {
    let mut journal = Vec::new();
    StateTransition::encode(
        &mut journal,
        StateTransitionArgs {
            prev_state: &entry.state,
            prev_lane_tip: &entry.lane_tip,
            new_state: &exit.state,
            new_lane_tip: &exit.lane_tip,
            new_seq_commit: &seq_commit(),
            covenant_id: &[0u8; 32],
            tx_image_id: &TX_IMAGE_ID,
            batch_image_id: &BATCH_IMAGE_ID,
            permission_spk_hash: &permission_spk_hash,
            deposit_spk_hash: &[0u8; 32],
            lane_key: &Hash::default(),
        },
    );
    journal
}

/// The aggregate a fresh proof of the bundle yields.
fn fresh_aggregate() -> Vec<u8> {
    aggregate(entry(), exit(), [0u8; 32])
}

/// The lane proof every fetch is served with. Its hashes feed only the `new_seq_commit` the
/// aggregator derives, and the SMT proof is a structurally valid empty one: an all-set bitmap
/// with every sibling elided, plus the `Full` terminal tag.
fn lane_proof() -> GetSeqCommitLaneProofResponse {
    let mut smt_proof = vec![0xFFu8; 33];
    smt_proof[32] = 0;
    GetSeqCommitLaneProofResponse {
        smt_proof,
        lane: None,
        payload_and_ctx_digest: Hash::default(),
        parent_seq_commit: Hash::default(),
        inactivity_shortcut: Hash::default(),
    }
}

/// Exit accumulator of a bundle whose batches emit no exit.
struct NoExits;

impl ExitAccumulator for NoExits {
    fn add_exit(&mut self, _dest: StandardSpk<'_>, _amount: u64) {
        unreachable!("the test batches emit no exit")
    }

    fn finalize(&self) -> [u8; 32] {
        [0u8; 32]
    }
}

/// Stands in for the guest's proof verification of a batch journal, which the host run skips.
fn skip_verify(_image_id: &[u8; 32], _journal: &[u8]) {}

/// The settlement journal the aggregator guest commits for a bundle over the two test batches.
/// This runs the guest's own verifier on the host, so it is what production caches for the
/// bundle, not a hand-written stand-in.
fn guest_aggregate() -> Vec<u8> {
    let journals = [batch_receipt(entry(), middle()), batch_receipt(middle(), exit())];
    let inputs = AggregatorInputs::encode(&BATCH_IMAGE_ID, &lane_proof(), &journals);
    let mut verifier = Verifier::new(&inputs, skip_verify, NoExits);
    let last = verifier.verify_batches();
    let mut journal = Vec::new();
    verifier.commit_state_transition(&mut journal, last);
    journal
}

/// Backend standing in for the aggregator guest. It counts every aggregator prove and composes
/// the journal the way the guest does: the bundle enters where the first batch receipt does and
/// exits where the last one does. Like the guest, it fails the proof over a batch journal that
/// does not decode.
#[derive(Clone, Default)]
struct ComposingBackend {
    /// Aggregator proves run so far, shared with the worker's clone.
    proves: Arc<AtomicUsize>,
}

impl vprogs_zk_transaction_prover::Backend for ComposingBackend {
    type Receipt = Vec<u8>;

    fn image_id(&self) -> &[u8; 32] {
        &TX_IMAGE_ID
    }

    fn prove_transaction(
        &self,
        _input_bytes: Vec<u8>,
    ) -> impl Future<Output = Result<Vec<u8>, String>> + Send + 'static {
        async { unreachable!("the tests prove no transaction") }
    }
}

impl vprogs_zk_batch_prover::Backend for ComposingBackend {
    fn prove_batch(
        &self,
        _inputs: &[u8],
        _receipts: Vec<Vec<u8>>,
    ) -> impl Future<Output = Result<Vec<u8>, String>> + Send + 'static {
        async { unreachable!("the tests publish the batch receipts directly") }
    }

    fn journal_bytes(receipt: &Vec<u8>) -> Vec<u8> {
        receipt.clone()
    }

    fn batch_image_id(&self) -> &[u8; 32] {
        &BATCH_IMAGE_ID
    }
}

impl vprogs_zk_aggregate_prover::Backend for ComposingBackend {
    fn prove_aggregator(
        &self,
        _inputs: &[u8],
        batch_receipts: Vec<Vec<u8>>,
    ) -> impl Future<Output = Result<Vec<u8>, String>> + Send + 'static {
        self.proves.fetch_add(1, Ordering::SeqCst);
        async move {
            let first = batch_receipts.first().expect("a bundle composes at least one batch");
            let last = batch_receipts.last().expect("a bundle composes at least one batch");
            let (Ok(first), Ok(last)) =
                (BatchTransition::ref_from_bytes(first), BatchTransition::ref_from_bytes(last))
            else {
                return Err("batch journal does not decode".to_owned());
            };
            Ok(aggregate(
                Pins { state: first.prev_state, lane_tip: first.prev_lane_tip },
                Pins { state: last.new_state, lane_tip: last.new_lane_tip },
                [0u8; 32],
            ))
        }
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
        Ok(lane_proof())
    }
}

/// Processor executing every transaction without touching resource bytes.
#[derive(Clone)]
struct PlainProcessor;

impl Processor<RocksDbStore> for PlainProcessor {
    fn process_transaction(
        &self,
        _ctx: &mut TransactionContext<RocksDbStore, Self>,
    ) -> Result<(), Self::Error> {
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

/// One lane transaction, so its batch is non-empty and brings a receipt into the bundle.
fn lane_tx(resource: usize) -> SchedulerTransaction<usize> {
    SchedulerTransaction::new(0, vec![AccessMetadata::write(ResourceId::for_test(resource))], 0)
}

/// Runs `future` to completion on a runtime of its own.
fn block_on<F: Future>(future: F) -> F::Output {
    Builder::new_current_thread().enable_time().build().expect("runtime").block_on(future)
}

/// An aggregate prover over two committed batches that form one bundle.
struct Harness {
    /// Keeps the store's directory alive.
    _temp_dir: TempDir,
    /// Scheduler the batches were committed through.
    scheduler: Scheduler<RocksDbStore, PlainProcessor>,
    /// The aggregate prover under test.
    prover: AggregateProver<RocksDbStore, PlainProcessor>,
    /// Bundles the aggregate prover emitted.
    queue: AsyncQueue<ScheduledBundle<SettlementArtifact<Vec<u8>>>>,
    /// Aggregator proves the worker ran, shared with its backend.
    proves: Arc<AtomicUsize>,
}

impl Harness {
    /// Commits two batches carrying the receipts `first` and `last`, puts `cached` at their
    /// bundle's coordinate in the aggregate cache and hands both batches to the aggregate
    /// prover.
    fn start(first: Vec<u8>, last: Vec<u8>, cached: Option<Vec<u8>>) -> Self {
        let temp_dir = TempDir::new().expect("failed to create temp dir");
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(PlainProcessor),
            StorageConfig::default().with_store(storage),
        );

        // Two committed batches. Their receipts are published the way the batch prover would.
        let batches = [
            scheduler.schedule(block(1, 0), vec![lane_tx(1)]),
            scheduler.schedule(block(2, 1), vec![lane_tx(2)]),
        ];
        for (batch, receipt) in batches.iter().zip([first, last]) {
            batch.wait_committed_blocking();
            batch.publish_artifact(Some(receipt));
        }

        // The cache entry an earlier bundle over the same range left at the coordinate.
        if let Some(cached) = cached {
            scheduler
                .state()
                .receipt_store()
                .write_agg_receipt(bundle_key(), cached)
                .wait_blocking();
        }

        let backend = ComposingBackend::default();
        let proves = backend.proves.clone();
        let queue: AsyncQueue<ScheduledBundle<SettlementArtifact<Vec<u8>>>> = AsyncQueue::new();
        let prover = AggregateProver::new(
            backend,
            scheduler.state().receipt_store(),
            None,
            AggregateProverConfig {
                lane_key: Hash::default(),
                covenant_id: None,
                lane_source: ServeLaneProofs,
                settlement_queue: Some(queue.clone()),
                settlement: None,
                re_root: None,
                bundle_size: 2..=2,
                exits: None,
                tip_daa: None,
                follow_depth: 0,
            },
        );
        for batch in &batches {
            prover.submit(batch);
        }
        Self { _temp_dir: temp_dir, scheduler, prover, queue, proves }
    }

    /// Reads the aggregate the cache holds at the bundle's coordinate.
    fn cached(&self) -> Option<Vec<u8>> {
        block_on(self.scheduler.state().receipt_store().read_agg_receipt(bundle_key()).resolve())
    }

    /// Shuts the prover and the scheduler down.
    fn shutdown(self) {
        self.prover.shutdown();
        self.scheduler.shutdown();
    }
}

/// The aggregate cache key of the bundle over both test batches.
fn bundle_key() -> AggregatorKey {
    let blocks = BundleBlocks { from_block: block_hash(1), block_prove_to: block_hash(2) };
    ScheduledBundle::<()>::new(2, 1, blocks).agg_key(AGGREGATOR_IMAGE_ID, seq_commit().as_bytes())
}

/// What the worker made of the bundle.
struct Outcome {
    /// The artifact the bundle was published with.
    artifact: Arc<SettlementArtifact<Vec<u8>>>,
    /// Aggregator proves the worker ran.
    proves: usize,
    /// The aggregate the cache holds at the bundle's coordinate afterwards.
    cached: Option<Vec<u8>>,
}

/// Bundles two batches whose receipts chain from [`entry`] over [`middle`] to [`exit`], with
/// `cached` at the bundle's coordinate in the aggregate cache, and returns what the worker made
/// of it.
///
/// The bundle spans two batches so that its entry and its exit come from different receipts: a
/// compare of the cached aggregate against the wrong one of the two cannot pass.
fn bundle_over(cached: Option<Vec<u8>>) -> Outcome {
    let harness =
        Harness::start(batch_receipt(entry(), middle()), batch_receipt(middle(), exit()), cached);

    // The handle is emitted before the worker parses the receipt's journal, so a worker that
    // dies on the parse leaves it unresolved: the wait is bounded.
    let deadline = Instant::now() + Duration::from_secs(10);
    let bundle = loop {
        if let Some(bundle) = harness.queue.pop() {
            break bundle;
        }
        assert!(Instant::now() < deadline, "the two batches must bundle");
        thread::sleep(Duration::from_millis(5));
    };
    block_on(async { timeout(Duration::from_secs(10), bundle.wait_artifact_published()).await })
        .expect("the worker must resolve the bundle it emitted");
    assert_eq!(bundle.checkpoint_index(), 1, "the bundle starts at the first batch");
    assert_eq!(bundle.block_prove_to(), block_hash(2), "and proves through the last one");

    let outcome = Outcome {
        artifact: bundle.artifact().expect("the bundle settles a state transition"),
        proves: harness.proves.load(Ordering::SeqCst),
        cached: harness.cached(),
    };
    harness.shutdown();
    outcome
}

/// Asserts that the bundle's artifact runs from the first batch receipt's entry to the last
/// one's exit.
fn assert_composed(artifact: &SettlementArtifact<Vec<u8>>) {
    assert_eq!(artifact.prev_state, entry().state, "it enters at the first batch's state");
    assert_eq!(artifact.prev_lane_tip, entry().lane_tip, "and at its lane tip");
    assert_eq!(artifact.new_state, exit().state, "it exits at the last batch's state");
    assert_eq!(artifact.new_lane_tip, exit().lane_tip, "and at its lane tip");
}

/// Asserts that a bundle finding `stale` cached at its coordinate is proved afresh: the worker
/// runs one aggregator prove, publishes the aggregate composed from the batch receipts, and
/// that aggregate replaces the stale one in the cache.
fn assert_proved_afresh(stale: Vec<u8>) {
    let outcome = bundle_over(Some(stale));
    assert_eq!(outcome.proves, 1, "the stale aggregate is not reused");
    assert_eq!(outcome.artifact.receipt, fresh_aggregate(), "the bundle carries the fresh one");
    assert_composed(&outcome.artifact);
    assert_eq!(outcome.cached, Some(fresh_aggregate()), "which replaces the stale one");
}

/// A cached aggregate entering at another state root was composed from other batch receipts:
/// the bundle is proved afresh.
#[test]
fn cached_aggregate_entering_at_another_state_is_proved_afresh() {
    let stale_entry = Pins { state: elsewhere().state, ..entry() };
    assert_proved_afresh(aggregate(stale_entry, exit(), [0u8; 32]));
}

/// A cached aggregate entering at another lane tip is proved afresh, although it enters at the
/// right state root.
#[test]
fn cached_aggregate_entering_at_another_lane_tip_is_proved_afresh() {
    let stale_entry = Pins { lane_tip: elsewhere().lane_tip, ..entry() };
    assert_proved_afresh(aggregate(stale_entry, exit(), [0u8; 32]));
}

/// A cached aggregate exiting at another state root is proved afresh, although it enters where
/// the bundle does.
#[test]
fn cached_aggregate_exiting_at_another_state_is_proved_afresh() {
    let stale_exit = Pins { state: elsewhere().state, ..exit() };
    assert_proved_afresh(aggregate(entry(), stale_exit, [0u8; 32]));
}

/// A cached aggregate exiting at another lane tip is proved afresh, although it enters where
/// the bundle does.
#[test]
fn cached_aggregate_exiting_at_another_lane_tip_is_proved_afresh() {
    let stale_exit = Pins { lane_tip: elsewhere().lane_tip, ..exit() };
    assert_proved_afresh(aggregate(entry(), stale_exit, [0u8; 32]));
}

/// A cached aggregate whose journal does not decode says nothing about the bundle: the worker
/// proves the bundle afresh instead of dying on the decode.
#[test]
fn cached_aggregate_that_does_not_decode_is_proved_afresh() {
    assert_proved_afresh(vec![0xFF; 7]);
}

/// A cached aggregate that enters where the first batch receipt does and exits where the last
/// one does is still reused: the worker publishes it without proving, which is what the cache
/// is for on a replay.
#[test]
fn cached_aggregate_composing_the_batch_receipts_is_reused() {
    let cached = aggregate(entry(), exit(), CACHED_MARK);
    let outcome = bundle_over(Some(cached.clone()));

    assert_eq!(outcome.proves, 0, "the cached aggregate is reused without a proof");
    assert_eq!(outcome.artifact.receipt, cached, "the bundle carries the cached aggregate");
    assert_eq!(outcome.artifact.permission_spk_hash, CACHED_MARK);
    assert_composed(&outcome.artifact);
    assert_eq!(outcome.cached, Some(cached), "and the cache keeps it");
}

/// The aggregate the aggregator guest commits for a bundle over the batch receipts composes
/// them: found in the cache, it is reused without a proof. The cached aggregate comes from the
/// guest's own verifier, so the worker's check cannot drift from what the guest commits without
/// failing here, and the backend of the other tests composes the same journal.
#[test]
fn aggregate_the_guest_commits_over_the_batch_receipts_is_reused() {
    let cached = guest_aggregate();
    assert_eq!(cached, fresh_aggregate(), "the test backend composes what the guest commits");
    let outcome = bundle_over(Some(cached.clone()));

    assert_eq!(outcome.proves, 0, "the guest's aggregate is reused without a proof");
    assert_eq!(outcome.artifact.receipt, cached, "the bundle carries the cached aggregate");
    assert_composed(&outcome.artifact);
}

/// With nothing cached the bundle is proved and its aggregate cached, as before.
#[test]
fn uncached_bundle_is_proved_and_cached() {
    let outcome = bundle_over(None);

    assert_eq!(outcome.proves, 1);
    assert_composed(&outcome.artifact);
    assert_eq!(outcome.cached, Some(fresh_aggregate()));
}

/// Batch receipts whose journals do not decode cannot vouch for a cached aggregate, however
/// well formed it is: the worker goes on to the proof, which fails as it does in the guest,
/// instead of serving the cached aggregate over them.
#[test]
fn cached_aggregate_is_not_served_over_batch_receipts_that_do_not_decode() {
    let harness = Harness::start(vec![0xFF; 7], vec![0xFF; 7], Some(fresh_aggregate()));

    // Every proof attempt fails, so the worker never emits the bundle. The first thing it does
    // is either the proof or serving the cached aggregate.
    let deadline = Instant::now() + Duration::from_secs(10);
    while harness.proves.load(Ordering::SeqCst) == 0 {
        assert!(harness.queue.pop().is_none(), "the cached aggregate must not be served");
        assert!(Instant::now() < deadline, "the worker never reached the bundle");
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(harness.cached(), Some(fresh_aggregate()), "the failed proof caches nothing");
    harness.shutdown();
}
