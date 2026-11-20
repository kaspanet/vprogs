//! Pins that a batch publishing its receipt while the worker's passes run still wakes the run
//! loop.
//!
//! The run loop parks on the publication of the first queued batch that is not published yet. It
//! used to pick that batch at the park, after the passes had already read what is published. A
//! batch that published in between was skipped as published, and its publication woke nothing:
//! the loop parked until the next command, with ready work in the queue.
//!
//! The window is held open here with a lane-proof fetch: the first bundle's fetch is in flight
//! while the next batch publishes, and then fails, so the pass ends without progress and the loop
//! parks.

// The backend traits return `impl Future + 'static`, which an `async fn` cannot satisfy: its future
// borrows `&self`.
#![allow(clippy::manual_async_fn)]

use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use kaspa_hashes::Hash;
use kaspa_rpc_core::GetSeqCommitLaneProofResponse;
use tempfile::TempDir;
use tokio::sync::Notify;
use vprogs_core_atomics::AsyncQueue;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId, SchedulerTransaction};
use vprogs_l1_types::ChainBlockMetadata;
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler, TransactionContext};
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
/// artifact instead of resolving the bundle as a no-op.
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
    ) -> impl Future<Output = Result<Self::Receipt, String>> + Send + 'static {
        async { unreachable!("the repro publishes batch receipts directly") }
    }

    type Receipt = Vec<u8>;
}

impl vprogs_zk_batch_prover::Backend for SyntheticBackend {
    fn prove_batch(
        &self,
        _inputs: &[u8],
        _receipts: Vec<Self::Receipt>,
    ) -> impl Future<Output = Result<Self::Receipt, String>> + Send + 'static {
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
    ) -> impl Future<Output = Result<Self::Receipt, String>> + Send + 'static {
        async { Ok(settlement_journal()) }
    }

    fn aggregator_image_id(&self) -> &[u8; 32] {
        &AGGREGATOR_IMAGE_ID
    }
}

/// Lane source whose first fetch stays in flight until the test releases it and then fails, the
/// way the production source reports a node that did not answer. Every later fetch is served.
struct HeldFirstFetch {
    /// Whether the first fetch was taken.
    taken: AtomicBool,
    /// Tells the test that the first fetch is in flight.
    reached: mpsc::Sender<()>,
    /// Lets the first fetch fail once the test notifies it.
    release: Arc<Notify>,
}

impl LaneProofSource for HeldFirstFetch {
    async fn fetch_lane_proof(
        &self,
        _req: LaneProofRequest,
    ) -> Result<GetSeqCommitLaneProofResponse, LaneProofError> {
        if !self.taken.swap(true, Ordering::SeqCst) {
            self.reached.send(()).expect("the test waits for the first fetch");
            self.release.notified().await;
            return Err(LaneProofError("node did not answer".into()));
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

/// Chain-block metadata for a block carrying the journal's `seq_commit`, which the worker checks
/// the bundle's journal against.
fn block(hash: u8, parent_id: u64) -> ChainBlockMetadata {
    ChainBlockMetadata {
        hash: Hash::from_bytes([hash; 32]),
        parent_id,
        seq_commit: seq_commit(),
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

/// Tests that a batch which publishes its receipt while a pass is running wakes the worker like
/// one that publishes after the worker parked.
///
/// Two batches are queued. The first one's bundle is formed and its lane-proof fetch is in
/// flight when the second batch publishes. The fetch then fails, the bundle is deferred and the
/// worker parks. The second batch's publication must end that park: the deferred bundle is
/// formed again, its fetch is served, and both bundles are emitted. Missing it leaves two
/// published batches queued until some later command.
#[test]
fn a_batch_published_during_a_pass_wakes_the_worker() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(PlainProcessor),
            StorageConfig::default().with_store(storage),
        );

        let (reached, fetch_in_flight) = mpsc::channel();
        let release = Arc::new(Notify::new());
        let settlement_queue: AsyncQueue<ScheduledBundle<SettlementArtifact<Vec<u8>>>> =
            AsyncQueue::new();
        let prover = AggregateProver::new(
            SyntheticBackend,
            scheduler.state().receipt_store(),
            None,
            AggregateProverConfig {
                lane_key: Hash::default(),
                covenant_id: None,
                lane_source: HeldFirstFetch {
                    taken: AtomicBool::new(false),
                    reached,
                    release: release.clone(),
                },
                settlement_queue: Some(settlement_queue.clone()),
                settlement: None,
                re_root: None,
                bundle_size: 1..=1,
                exits: None,
                tip_daa: None,
                follow_depth: 0,
            },
        );

        // Both batches are committed and queued before either publishes its receipt, so the
        // worker holds the second one when the first one's bundle is formed.
        let first = scheduler.schedule(block(1, 0), vec![lane_tx()]);
        first.wait_committed_blocking();
        let second = scheduler.schedule(block(2, 1), vec![lane_tx()]);
        second.wait_committed_blocking();
        prover.submit(&first);
        prover.submit(&second);
        // Let the worker drain both submits and park. A submit that lands while the worker is
        // running leaves a wake-up behind for its next park, and that one spare wake-up would
        // stand in for the publication this test is about.
        thread::sleep(Duration::from_millis(250));

        // The first batch publishes: its bundle is formed and the lane-proof fetch starts.
        first.publish_artifact(Some(settlement_journal()));
        fetch_in_flight
            .recv_timeout(Duration::from_secs(10))
            .expect("the first bundle must reach its lane-proof fetch");

        // The second batch publishes while the fetch is in flight. Then the fetch fails, so the
        // pass ends without progress and the worker parks.
        second.publish_artifact(Some(settlement_journal()));
        release.notify_one();

        // The publication must have woken the worker: the deferred bundle is formed again and
        // the second batch's bundle follows it.
        for block in [1, 2] {
            let bundle = next_bundle(&settlement_queue, Duration::from_secs(10))
                .expect("the batch published during the pass must wake the worker");
            assert_eq!(bundle.block_prove_to(), block_hash(block));
        }

        prover.shutdown();
        scheduler.shutdown();
    }
}
