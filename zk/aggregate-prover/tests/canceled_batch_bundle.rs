//! Reproduces the aggregate-prover panic when a bundle sweeps in a batch that a rollback canceled
//! while it was still executing.
//!
//! A batch whose last transaction finishes after the cancellation has its `artifact_published`
//! latch force-opened without a receipt being stored. The bundle extend loop reads that latch as
//! "receipt ready" and includes the batch, and the receipt collection then panics with "batch
//! artifact not ready", killing the worker thread: bundling and settlement stop for the rest of the
//! process's life.

// The backend traits return `impl Future + 'static`, which an `async fn` cannot satisfy: its future
// borrows `&self`.
#![allow(clippy::manual_async_fn)]

use std::{
    future::Future,
    thread,
    time::{Duration, Instant},
};

use kaspa_hashes::Hash;
use kaspa_rpc_core::GetSeqCommitLaneProofResponse;
use tempfile::TempDir;
use vprogs_core_atomics::{AsyncQueue, AtomicAsyncLatch};
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId, SchedulerTransaction};
use vprogs_l1_types::ChainBlockMetadata;
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler, TransactionContext};
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_aggregate_prover::{
    AggregateProver, AggregateProverConfig, ScheduledBundle, SettlementArtifact,
};
use vprogs_zk_batch_prover::{LaneProofRequest, LaneProofSource};

/// Transaction payload whose execution parks until the test releases it.
const GATE_TX: usize = 100;

/// Transaction-guest image id. This repro proves nothing, so it only keys receipt-cache lookups.
const TX_IMAGE_ID: [u8; 32] = [0u8; 32];
/// Batch-guest image id, keying a per-batch receipt in the proof-receipt store.
const BATCH_IMAGE_ID: [u8; 32] = [1u8; 32];
/// Aggregator-guest image id, keying a bundle's settlement receipt.
const AGGREGATOR_IMAGE_ID: [u8; 32] = [2u8; 32];

/// Backend whose proving methods are unreachable. Every bundle this repro forms is all-empty (the
/// live front is an empty batch), so the worker composes no receipt and proves nothing; reaching
/// any of these means the canceled batch was swept into a bundle.
#[derive(Clone)]
struct NoopBackend;

impl vprogs_zk_transaction_prover::Backend for NoopBackend {
    fn image_id(&self) -> &[u8; 32] {
        &TX_IMAGE_ID
    }

    fn prove_transaction(
        &self,
        _input_bytes: Vec<u8>,
    ) -> impl Future<Output = Self::Receipt> + Send + 'static {
        async { unreachable!("the repro proves no transaction") }
    }

    type Receipt = Vec<u8>;
}

impl vprogs_zk_batch_prover::Backend for NoopBackend {
    fn prove_batch(
        &self,
        _inputs: &[u8],
        _receipts: Vec<Self::Receipt>,
    ) -> impl Future<Output = Self::Receipt> + Send + 'static {
        async { unreachable!("the repro proves no batch") }
    }

    fn journal_bytes(_receipt: &Self::Receipt) -> Vec<u8> {
        unreachable!("the repro proves no batch")
    }

    fn batch_image_id(&self) -> &[u8; 32] {
        &BATCH_IMAGE_ID
    }
}

impl vprogs_zk_aggregate_prover::Backend for NoopBackend {
    fn prove_aggregator(
        &self,
        _inputs: &[u8],
        _batch_receipts: Vec<Self::Receipt>,
    ) -> impl Future<Output = Self::Receipt> + Send + 'static {
        async { unreachable!("the repro proves no bundle") }
    }

    fn aggregator_image_id(&self) -> &[u8; 32] {
        &AGGREGATOR_IMAGE_ID
    }
}

/// Lane source for a prover that never reaches the proving path.
struct NoLaneProofs;

impl LaneProofSource for NoLaneProofs {
    async fn fetch_lane_proof(&self, _req: LaneProofRequest) -> GetSeqCommitLaneProofResponse {
        unreachable!("the repro proves no bundle, so it fetches no lane proof")
    }
}

/// Processor that parks the execution of [`GATE_TX`] until `release` opens, signalling entry on
/// `entered` so the test knows the transaction is in flight.
#[derive(Clone)]
struct GateProcessor {
    /// Opened by the processor once the gated transaction is inside execution.
    entered: AtomicAsyncLatch,
    /// Opened by the test to let the gated transaction finish.
    release: AtomicAsyncLatch,
}

impl vprogs_scheduling_scheduler::Processor<RocksDbStore> for GateProcessor {
    fn process_transaction(
        &self,
        ctx: &mut TransactionContext<RocksDbStore, Self>,
    ) -> Result<(), Self::Error> {
        if ctx.scheduler_tx().tx == GATE_TX {
            self.entered.open();
            self.release.wait_blocking();
        }
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

/// Tests that a batch canceled mid-execution is never bundled.
///
/// Cancellation force-opens the canceled batch's artifact latch without storing a receipt, so the
/// extend loop's `artifact_published()` check reads it as provable and sweeps it in behind the live
/// front; collecting its receipt then panics and kills the aggregate worker.
///
/// The rollback reaches the scheduler but not the prover's inbox: in production the worker is
/// parked on the front's artifact latch when the reorg lands and cannot drain `Command::Rollback`
/// until it returns from bundling, which is the very call that panics.
#[test]
fn canceled_batch_is_not_swept_into_a_bundle() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let entered = AtomicAsyncLatch::new();
        let release = AtomicAsyncLatch::new();
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(GateProcessor {
                entered: entered.clone(),
                release: release.clone(),
            }),
            StorageConfig::default().with_store(storage),
        );

        // The bundle's front: an empty batch, live and published from creation, as every chain
        // block without lane transactions is. It gives the rollback a committed target too.
        let front = scheduler.schedule(block(1, 0), vec![]);
        front.wait_committed_blocking();
        assert!(front.artifact_published(), "an empty batch publishes at creation");

        // The batch behind it parks inside execution, so the rollback below lands while its only
        // transaction is still in flight.
        let canceled = scheduler.schedule(
            block(2, 1),
            vec![SchedulerTransaction::new(
                0,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                GATE_TX,
            )],
        );
        entered.wait_blocking();

        scheduler.rollback_to(1).expect("rollback should succeed");
        assert!(canceled.canceled(), "the in-flight batch should be canceled");
        assert!(!front.canceled(), "the front should survive the rollback");

        // Releasing the gate lets the canceled batch's last transaction finish, which force-opens
        // its artifact latch without ever storing a receipt.
        release.open();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !canceled.artifact_published() {
            assert!(Instant::now() < deadline, "the canceled batch should force-open its latch");
            thread::sleep(Duration::from_millis(20));
        }

        let settlement_queue: AsyncQueue<ScheduledBundle<SettlementArtifact<Vec<u8>>>> =
            AsyncQueue::new();
        let prover = AggregateProver::new(
            NoopBackend,
            scheduler.state().receipt_store(),
            AggregateProverConfig {
                lane_key: Hash::default(),
                covenant_id: None,
                lane_source: NoLaneProofs,
                settlement_queue: Some(settlement_queue.clone()),
                settlement: None,
                bundle_size: 1..=usize::MAX,
            },
        );
        prover.submit(&front);
        prover.submit(&canceled);

        // The front bundles alone: the canceled batch carries no receipt to compose.
        let bundle = next_bundle(&settlement_queue, Duration::from_secs(5))
            .expect("the live front must bundle without the canceled batch behind it");
        assert_eq!(bundle.batches(), 1, "the canceled batch must not be swept into the bundle");
        assert_eq!(bundle.checkpoint_index(), 1, "the bundle starts at the front");

        // A worker that panicked collecting the canceled batch's receipt never serves this batch.
        let probe = scheduler.schedule(block(3, 1), vec![]);
        prover.submit(&probe);
        let bundle = next_bundle(&settlement_queue, Duration::from_secs(5))
            .expect("the aggregate worker must survive the canceled batch and keep bundling");
        assert_eq!(bundle.checkpoint_index(), probe.checkpoint().index());

        prover.shutdown();
        scheduler.shutdown();
    }
}

/// Tests that the worker evicts an entirely-canceled queue and keeps bundling afterwards.
///
/// The queue holds two canceled batches: the front finished executing before the rollback, and the
/// one behind it parked inside execution so the rollback landed mid-flight, force-opening its
/// artifact latch without a receipt. Both must be evicted, never bundled, and a live batch
/// submitted afterwards must still bundle.
///
/// Only one direction is guaranteed, and only it is asserted: a force-opened latch always belongs
/// to a canceled batch. The converse does not hold, so the front's latch may be open or closed
/// depending on how the rollback interleaves with its final `decrease_pending_txs`.
#[test]
fn whole_canceled_queue_is_evicted() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let entered = AtomicAsyncLatch::new();
        let release = AtomicAsyncLatch::new();
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(GateProcessor {
                entered: entered.clone(),
                release: release.clone(),
            }),
            StorageConfig::default().with_store(storage),
        );

        // A committed anchor gives the rollback a target below every queued batch.
        let anchor = scheduler.schedule(block(1, 0), vec![]);
        anchor.wait_committed_blocking();

        // The future front finishes executing before the rollback. Its artifact latch is left
        // unasserted: `decrease_pending_txs` opens `processed` before it reads `canceled()`, so a
        // rollback landing in that gap force-opens the latch of a batch that had already finished.
        let never_published = scheduler.schedule(
            block(2, 1),
            vec![SchedulerTransaction::new(
                0,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                0,
            )],
        );
        never_published.wait_processed_blocking();

        // The batch behind it parks inside execution so the rollback lands mid-flight; finishing
        // afterwards force-opens its artifact latch without a receipt.
        let force_opened = scheduler.schedule(
            block(3, 2),
            vec![SchedulerTransaction::new(
                0,
                vec![AccessMetadata::write(ResourceId::for_test(2))],
                GATE_TX,
            )],
        );
        entered.wait_blocking();

        scheduler.rollback_to(1).expect("rollback should succeed");
        assert!(never_published.canceled(), "the processed batch should be canceled");
        assert!(force_opened.canceled(), "the in-flight batch should be canceled");

        release.open();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !force_opened.artifact_published() {
            assert!(Instant::now() < deadline, "the canceled batch should force-open its latch");
            thread::sleep(Duration::from_millis(20));
        }

        let settlement_queue: AsyncQueue<ScheduledBundle<SettlementArtifact<Vec<u8>>>> =
            AsyncQueue::new();
        let prover = AggregateProver::new(
            NoopBackend,
            scheduler.state().receipt_store(),
            AggregateProverConfig {
                lane_key: Hash::default(),
                covenant_id: None,
                lane_source: NoLaneProofs,
                settlement_queue: Some(settlement_queue.clone()),
                settlement: None,
                bundle_size: 1..=usize::MAX,
            },
        );
        prover.submit(&never_published);
        prover.submit(&force_opened);

        // A live batch scheduled after the rollback must bundle: the worker survived evicting the
        // canceled queue without bundling (or panicking on) either canceled batch.
        let probe = scheduler.schedule(block(4, 1), vec![]);
        prover.submit(&probe);
        let bundle = next_bundle(&settlement_queue, Duration::from_secs(5))
            .expect("the worker must evict the canceled queue and keep bundling");
        assert_eq!(bundle.batches(), 1, "no canceled batch may be swept into the probe's bundle");
        assert_eq!(bundle.checkpoint_index(), probe.checkpoint().index());

        prover.shutdown();
        scheduler.shutdown();
    }
}
