//! Pins the committed-gap pass's retry semantics: a lane-proof fetch that fails at startup (the
//! node stalled while the worker booted) must defer the gap range and retry it on later loop
//! wakes instead of leaving it uncovered for the life of the process, because nothing else ever
//! re-schedules a committed batch. The recovered gap bundle must also land on the settlement
//! queue ahead of new work. A genuinely dead gap block (reorged away during the downtime) must
//! not wedge new work: its fetches stop once the journal tail settles past the gap, and bundles
//! for live blocks keep emitting.

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
use tokio::sync::watch;
use vprogs_core_atomics::AsyncQueue;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId, SchedulerTransaction};
use vprogs_l1_types::{ChainBlockMetadata, SettlementInfo};
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler, TransactionContext};
use vprogs_state_settlement_journal::{SettlementJournal, StoreJournal};
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_abi::batch_aggregator::{StateTransition, StateTransitionArgs};
use vprogs_zk_aggregate_prover::{
    AggregateProver, AggregateProverConfig, ScheduledBundle, SettlementArtifact,
};
use vprogs_zk_batch_prover::{LaneProofError, LaneProofRequest, LaneProofSource};

/// Transaction-guest image id. This repro proves nothing real, so image ids only key receipt
/// lookups.
const TX_IMAGE_ID: [u8; 32] = [0u8; 32];
/// Batch-guest image id, keying a per-batch receipt in the proof-receipt store.
const BATCH_IMAGE_ID: [u8; 32] = [1u8; 32];
/// Aggregator-guest image id, keying a bundle's settlement receipt.
const AGGREGATOR_IMAGE_ID: [u8; 32] = [2u8; 32];

/// `seq_commit` the synthetic settlement journal derives. Every block's metadata carries it so the
/// worker's journal-vs-metadata check holds.
fn seq_commit() -> Hash {
    Hash::from_bytes([0x33; 32])
}

/// Block-hash helper keyed to the single byte every test block is built from.
fn block_hash(byte: u8) -> Hash {
    Hash::from_bytes([byte; 32])
}

/// Encodes the settlement journal the synthetic aggregator receipt carries: a real (non-no-op)
/// state transition whose `new_seq_commit` matches [`seq_commit`], so the worker publishes an
/// artifact instead of resolving the bundle as a no-op. Fields are encoded in declared order by the
/// journal's own encoder.
fn settlement_journal() -> Vec<u8> {
    let mut buf = Vec::new();
    StateTransition::encode(
        &mut buf,
        StateTransitionArgs {
            prev_state: &[0x00; 32],
            prev_lane_tip: &Hash::default(),
            new_state: &[0x11; 32],
            new_lane_tip: &Hash::default(),
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

/// Backend standing in for all three guests: the aggregator receipt is the settlement journal
/// itself (identity `journal_bytes`), so the worker parses exactly the transition above.
#[derive(Clone)]
struct SyntheticBackend;

impl vprogs_zk_transaction_prover::Backend for SyntheticBackend {
    fn image_id(&self) -> &[u8; 32] {
        &TX_IMAGE_ID
    }

    fn prove_transaction(
        &self,
        _input_bytes: Vec<u8>,
    ) -> impl Future<Output = Self::Receipt> + Send + 'static {
        async { unreachable!("the repro publishes batch receipts directly") }
    }

    type Receipt = Vec<u8>;
}

impl vprogs_zk_batch_prover::Backend for SyntheticBackend {
    fn prove_batch(
        &self,
        _inputs: &[u8],
        _receipts: Vec<Self::Receipt>,
    ) -> impl Future<Output = Self::Receipt> + Send + 'static {
        async { unreachable!("the repro publishes batch receipts directly") }
    }

    fn journal_bytes(receipt: &Self::Receipt) -> Vec<u8> {
        receipt.clone()
    }

    fn batch_image_id(&self) -> &[u8; 32] {
        &BATCH_IMAGE_ID
    }
}

impl vprogs_zk_aggregate_prover::Backend for SyntheticBackend {
    fn prove_aggregator(
        &self,
        _inputs: &[u8],
        _batch_receipts: Vec<Self::Receipt>,
    ) -> impl Future<Output = Self::Receipt> + Send + 'static {
        async { settlement_journal() }
    }

    fn aggregator_image_id(&self) -> &[u8; 32] {
        &AGGREGATOR_IMAGE_ID
    }
}

/// Lane source standing in for a node that stalls while the worker boots: the very first fetch
/// fails the way a timing-out RPC does, every later fetch serves.
struct StalledStartLaneSource {
    /// Fetches attempted so far, shared with the test; fetch zero fails.
    fetches: Arc<AtomicUsize>,
}

impl LaneProofSource for StalledStartLaneSource {
    async fn fetch_lane_proof(
        &self,
        _req: LaneProofRequest,
    ) -> Result<GetSeqCommitLaneProofResponse, LaneProofError> {
        if self.fetches.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(LaneProofError("request timed out".into()));
        }
        Ok(GetSeqCommitLaneProofResponse {
            smt_proof: Vec::new(),
            lane: None,
            payload_and_ctx_digest: Hash::default(),
            parent_seq_commit: Hash::default(),
            inactivity_shortcut: Hash::default(),
        })
    }
}

/// Lane source standing in for a block a reorg orphaned during the downtime: its fetch fails
/// forever, every live block's fetch serves, and the dead block's fetch count is observable.
struct DeadGapLaneSource {
    /// Hash of the block a reorg orphaned before the restart.
    dead: Hash,
    /// Fetches attempted for the dead block so far, shared with the test.
    dead_fetches: Arc<AtomicUsize>,
}

impl LaneProofSource for DeadGapLaneSource {
    async fn fetch_lane_proof(
        &self,
        req: LaneProofRequest,
    ) -> Result<GetSeqCommitLaneProofResponse, LaneProofError> {
        if req.block == self.dead {
            self.dead_fetches.fetch_add(1, Ordering::SeqCst);
            return Err(LaneProofError("block not found".into()));
        }
        Ok(GetSeqCommitLaneProofResponse {
            smt_proof: Vec::new(),
            lane: None,
            payload_and_ctx_digest: Hash::default(),
            parent_seq_commit: Hash::default(),
            inactivity_shortcut: Hash::default(),
        })
    }
}

/// Processor executing every transaction without touching resource bytes.
#[derive(Clone)]
struct PlainProcessor;

impl vprogs_scheduling_scheduler::Processor<RocksDbStore> for PlainProcessor {
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

/// Chain-block metadata for a block carrying the journal's `seq_commit` and an advancing lane tip,
/// so the committed-gap pass composes this batch's cached receipt.
fn block(hash: u8, parent_id: u64) -> ChainBlockMetadata {
    ChainBlockMetadata {
        hash: Hash::from_bytes([hash; 32]),
        parent_id,
        seq_commit: seq_commit(),
        prev_lane_tip: Hash::default(),
        lane_tip: Hash::from_bytes([hash; 32]),
        ..Default::default()
    }
}

/// One lane transaction: enough for the batch to be non-empty, so its bundle composes a receipt
/// and reaches the lane-proof fetch.
fn lane_tx() -> SchedulerTransaction<usize> {
    SchedulerTransaction::new(0, vec![AccessMetadata::write(ResourceId::for_test(1))], 0)
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

/// Spins until `count` returns at least `want`, so the test proceeds only after the worker's
/// startup gap pass has actually fetched (and the source has counted the attempt).
fn wait_for_fetches(count: &AtomicUsize, want: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while count.load(Ordering::SeqCst) < want {
        assert!(Instant::now() < deadline, "the worker never attempted the gap fetch");
        thread::sleep(Duration::from_millis(5));
    }
}

/// Commits a one-transaction batch for `meta`, seeds its cached per-batch receipt the way the
/// batch prover would have, and returns the batch handle. No aggregate prover is wired here: the
/// batch is committed but never journaled, which is exactly the committed gap the restarted
/// worker must cover.
fn commit_gap_batch(
    scheduler: &mut Scheduler<RocksDbStore, PlainProcessor>,
    meta: ChainBlockMetadata,
) -> vprogs_scheduling_scheduler::ScheduledBatch<RocksDbStore, PlainProcessor> {
    let batch = scheduler.schedule(meta, vec![lane_tx()]);
    batch.wait_committed_blocking();
    batch.write_batch_receipt(settlement_journal()).wait_blocking();
    batch
}

/// Commits a batch for `meta`, publishes its receipt, and feeds it to the aggregate prover as new
/// post-restart work.
fn commit_and_submit(
    scheduler: &mut Scheduler<RocksDbStore, PlainProcessor>,
    prover: &AggregateProver<RocksDbStore, PlainProcessor>,
    meta: ChainBlockMetadata,
) {
    let batch = scheduler.schedule(meta, vec![lane_tx()]);
    batch.wait_committed_blocking();
    batch.write_batch_receipt(settlement_journal()).wait_blocking();
    batch.publish_artifact(Some(settlement_journal()));
    prover.submit(&batch);
}

/// A settlement tip on a block no test batch carries, so the resumed gap pass treats the whole
/// committed span as uncovered (the boundary maps to no checkpoint).
fn unrelated_tip() -> SettlementInfo {
    SettlementInfo { block_prove_to: block_hash(0xff), ..Default::default() }
}

/// Tests that a committed gap whose lane-proof fetch fails at startup (the node stalled while the
/// worker booted) is retried on the next wake and settles ahead of the new work that arrived in
/// between, instead of staying uncovered for the life of the process.
#[test]
fn committed_gap_retries_after_a_transient_fetch_failure() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        // The pre-restart half: block 1's batch is committed with its receipt cached but no
        // journal entry, the state a kill between commit and journal record leaves behind.
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let journal = Arc::new(StoreJournal::new(storage.clone()));
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(PlainProcessor),
            StorageConfig::default().with_store(storage),
        );
        commit_gap_batch(&mut scheduler, block(1, 0));

        let settlement_queue: AsyncQueue<ScheduledBundle<SettlementArtifact<Vec<u8>>>> =
            AsyncQueue::new();
        let (settlement_tx, settlement_rx) = watch::channel::<Option<SettlementInfo>>(None);
        let fetches = Arc::new(AtomicUsize::new(0));
        let prover = AggregateProver::new(
            SyntheticBackend,
            scheduler.state().receipt_store(),
            AggregateProverConfig {
                lane_key: Hash::default(),
                covenant_id: None,
                lane_source: StalledStartLaneSource { fetches: fetches.clone() },
                settlement_queue: Some(settlement_queue.clone()),
                settlement: Some(settlement_rx),
                journal: Some(journal.clone()),
                bundle_size: 1..=1,
                exits: None,
            },
        );

        // Break the startup gate on a tip before any new work exists, so the startup gap pass
        // scopes to block 1 alone; its fetch is the stalled source's one failure.
        settlement_tx.send_replace(Some(unrelated_tip()));
        wait_for_fetches(&fetches, 1);

        // New work wakes the loop: the deferred gap must be retried on that wake and settle
        // first, with block 2's bundle behind it on the queue.
        commit_and_submit(&mut scheduler, &prover, block(2, 1));

        let gap = next_bundle(&settlement_queue, Duration::from_secs(10))
            .expect("the deferred gap must be retried and settle");
        gap.wait_artifact_published_blocking();
        assert_eq!(gap.block_prove_to(), block_hash(1), "the gap bundle covers block 1");
        assert!(gap.artifact().is_some(), "the gap bundle carries a real artifact");

        let new_work = next_bundle(&settlement_queue, Duration::from_secs(10))
            .expect("new work past the gap must settle after it");
        new_work.wait_artifact_published_blocking();
        assert_eq!(
            new_work.block_prove_to(),
            block_hash(2),
            "block 2's bundle settles after the recovered gap, never ahead of it",
        );
        assert!(new_work.artifact().is_some());

        // Exactly two handles were published, and the gap range is journaled.
        assert!(
            next_bundle(&settlement_queue, Duration::from_millis(500)).is_none(),
            "no further handle may arrive",
        );
        assert_eq!(
            journal.entries().len(),
            2,
            "the gap entry and block 2's entry are both recorded",
        );

        prover.shutdown();
        scheduler.shutdown();
    }
}

/// Tests that a genuinely dead gap block (a reorg orphaned it before the restart) does not wedge
/// the worker: no handle is ever published for it, new-work bundles keep settling, and the gap
/// fetches stop once the journal tail settles past the gap's bound.
#[test]
fn dead_gap_does_not_wedge_new_work() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let journal = Arc::new(StoreJournal::new(storage.clone()));
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(PlainProcessor),
            StorageConfig::default().with_store(storage),
        );
        commit_gap_batch(&mut scheduler, block(1, 0));

        let settlement_queue: AsyncQueue<ScheduledBundle<SettlementArtifact<Vec<u8>>>> =
            AsyncQueue::new();
        let (settlement_tx, settlement_rx) = watch::channel::<Option<SettlementInfo>>(None);
        let dead_fetches = Arc::new(AtomicUsize::new(0));
        let prover = AggregateProver::new(
            SyntheticBackend,
            scheduler.state().receipt_store(),
            AggregateProverConfig {
                lane_key: Hash::default(),
                covenant_id: None,
                lane_source: DeadGapLaneSource {
                    dead: block_hash(1),
                    dead_fetches: dead_fetches.clone(),
                },
                settlement_queue: Some(settlement_queue.clone()),
                settlement: Some(settlement_rx),
                journal: Some(journal.clone()),
                bundle_size: 1..=1,
                exits: None,
            },
        );

        settlement_tx.send_replace(Some(unrelated_tip()));
        wait_for_fetches(&dead_fetches, 1);

        // Block 2 is live: its bundle must settle even though block 1's gap fetch keeps failing.
        commit_and_submit(&mut scheduler, &prover, block(2, 1));
        let second = next_bundle(&settlement_queue, Duration::from_secs(10))
            .expect("new work past a dead gap must still settle");
        second.wait_artifact_published_blocking();
        assert_eq!(
            second.block_prove_to(),
            block_hash(2),
            "the first handle is block 2's bundle: the dead gap block never settles",
        );
        assert!(second.artifact().is_some());

        // Block 3 settles too, and the gap's retries stop once the journal tail (block 2's
        // entry) passes the dead gap's bound: the startup fetch, at least one retry, and no
        // fetch after the tail moved past.
        commit_and_submit(&mut scheduler, &prover, block(3, 2));
        let third = next_bundle(&settlement_queue, Duration::from_secs(10))
            .expect("the worker must keep settling past the dead gap");
        third.wait_artifact_published_blocking();
        assert_eq!(third.block_prove_to(), block_hash(3));
        assert!(third.artifact().is_some());

        assert!(
            next_bundle(&settlement_queue, Duration::from_millis(500)).is_none(),
            "no further handle may arrive: the dead gap block settles nothing",
        );
        assert!(
            dead_fetches.load(Ordering::SeqCst) >= 2,
            "the deferred gap must have been retried beyond the startup attempt",
        );

        prover.shutdown();
        scheduler.shutdown();
    }
}
