//! Pins the worker's bundle escalation over a failed aggregator proof attempt: an attempt that
//! fails (a prove error or a receipt the image id rejects) consumes nothing and counts as
//! run-loop progress, so the bundle is re-formed from the consecutively-ready prefix: strictly
//! longer when batches arrived while attempts kept failing, identical when none did.

// The backend traits return `impl Future + 'static`, which an `async fn` cannot satisfy: its future
// borrows `&self`.
#![allow(clippy::manual_async_fn)]

use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
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
use vprogs_state_settlement_journal::StoreJournal;
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_abi::{
    batch_aggregator::{StateTransition, StateTransitionArgs},
    batch_processor::{BatchTransition, BatchTransitionArgs},
};
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

/// Chain-block metadata for a block carrying the journal's `seq_commit`.
fn block(hash: u8, parent_id: u64) -> ChainBlockMetadata {
    ChainBlockMetadata {
        hash: block_hash(hash),
        parent_id,
        seq_commit: seq_commit(),
        prev_lane_tip: Hash::default(),
        lane_tip: block_hash(hash),
        ..Default::default()
    }
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

/// Encodes the per-batch receipt journal for a committed block's metadata: the synthetic
/// aggregator ignores its contents, so it only needs to exist for the batch to count as live.
fn batch_receipt_from(metadata: &ChainBlockMetadata) -> Vec<u8> {
    let hash = metadata.hash.as_bytes()[0];
    let mut buf = Vec::new();
    BatchTransition::encode(
        &mut buf,
        BatchTransitionArgs {
            prev_state: &[0x11; 32],
            prev_lane_tip: &Hash::from_bytes([hash.saturating_sub(1); 32]),
            prev_lane_blue_score: 0,
            new_state: &[0x11; 32],
            new_lane_tip: &Hash::from_bytes([hash; 32]),
            new_lane_blue_score: 0,
            lane_key: &Hash::default(),
            covenant_id: &[0u8; 32],
            tx_image_id: &TX_IMAGE_ID,
            deposit_spk_hash: &[0u8; 32],
            lane_expired: false,
            exits: b"",
        },
    );
    buf
}

/// Backend whose aggregator attempts fail while `failing` is set or the span holds fewer than
/// `succeed_from_len` batch receipts, counting attempts so a test can observe and release the
/// failure. The span rule is what makes a re-form test deterministic: the worker never parks
/// between failed attempts, so a time-based flag flip can land inside an in-flight formation
/// taken before the new batch was drained, while a rule keyed to the span's own receipts
/// fails that shorter formation no matter when it runs. Attempts never block the worker's
/// single-threaded runtime.
#[derive(Clone)]
struct FlakyBackend {
    attempts: Arc<AtomicUsize>,
    failing: Arc<AtomicBool>,
    succeed_from_len: usize,
}

impl vprogs_zk_transaction_prover::Backend for FlakyBackend {
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

impl vprogs_zk_batch_prover::Backend for FlakyBackend {
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

impl vprogs_zk_aggregate_prover::Backend for FlakyBackend {
    fn prove_aggregator(
        &self,
        _inputs: &[u8],
        batch_receipts: Vec<Self::Receipt>,
    ) -> impl Future<Output = Result<Self::Receipt, String>> + Send + 'static {
        let attempts = self.attempts.clone();
        let failing = self.failing.clone();
        let succeed_from_len = self.succeed_from_len;
        async move {
            attempts.fetch_add(1, Ordering::SeqCst);
            if failing.load(Ordering::SeqCst) || batch_receipts.len() < succeed_from_len {
                Err("synthetic invalid proof".to_owned())
            } else {
                Ok(settlement_journal())
            }
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

/// Polls `pred` every 10ms until it holds, panicking with `desc` once `timeout` elapses.
fn wait_until(desc: &str, timeout: Duration, mut pred: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !pred() {
        if Instant::now() >= deadline {
            panic!("timed out waiting for {desc}");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Harness holding the live pieces an escalation test drives: the flaky backend (its flag shared
/// with the worker), plus the temp dir backing its store.
struct Harness {
    /// Flaky backend clone sharing the attempt counter and failure flag with the worker.
    backend: FlakyBackend,
    /// Temp dir backing the harness's store.
    _temp: TempDir,
    /// Scheduler committing each test's batches.
    scheduler: Scheduler<RocksDbStore, PlainProcessor>,
    /// Prover whose worker re-forms the failed bundles.
    prover: AggregateProver<RocksDbStore, PlainProcessor>,
    /// Queue the worker's emitted bundle handles land on.
    queue: AsyncQueue<ScheduledBundle<SettlementArtifact<Vec<u8>>>>,
    /// Sender keeping the settlement watch alive; the tests never publish a settlement.
    _settlement_tx: watch::Sender<Option<SettlementInfo>>,
}

/// Opens the store, scheduler, and prover (bundle cap `1..=8`), sleeps out the worker's startup
/// for boot determinism, then makes batch 1 live: committed, receipt written and published, and
/// submitted, so the worker immediately starts failing attempts over it.
fn setup(failing: bool, succeed_from_len: usize) -> Harness {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
    let journal = StoreJournal::new(storage.clone());
    let mut scheduler = Scheduler::new(
        ExecutionConfig::default().with_processor(PlainProcessor),
        StorageConfig::default().with_store(storage),
    );
    let queue: AsyncQueue<ScheduledBundle<SettlementArtifact<Vec<u8>>>> = AsyncQueue::new();
    let (settlement_tx, settlement_rx) = watch::channel::<Option<SettlementInfo>>(None);
    let backend = FlakyBackend {
        attempts: Arc::new(AtomicUsize::new(0)),
        failing: Arc::new(AtomicBool::new(failing)),
        succeed_from_len,
    };
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
            bundle_size: 1..=8,
            exits: None,
        },
    );

    // Boot determinism: wait out the worker's startup so its committed-gap gate has passed
    // (empty journal, nothing committed) before the first batch commits.
    thread::sleep(Duration::from_millis(300));

    let batch = scheduler.schedule(block(1, 0), vec![lane_tx()]);
    batch.wait_committed_blocking();
    batch.write_batch_receipt(batch_receipt_from(batch.checkpoint().metadata())).wait_blocking();
    batch.publish_artifact(Some(settlement_journal()));
    prover.submit(&batch);

    Harness { backend, _temp: temp_dir, scheduler, prover, queue, _settlement_tx: settlement_tx }
}

/// A failed bundle attempt consumes nothing: batches that arrive while attempts keep failing
/// join the next formed span, so the retry proves a strictly longer bundle instead of failing
/// on identical inputs.
#[test]
fn a_failed_attempt_reforms_over_batches_that_arrived() {
    // The flag stays clear: every attempt over the batch-1 span alone fails on the span rule,
    // so no flag flip can race an in-flight short formation (the worker never parks between
    // failed attempts).
    let mut harness = setup(false, 2);
    wait_until("first failed attempt", Duration::from_secs(10), || {
        harness.backend.attempts.load(Ordering::SeqCst) >= 1
    });
    // Batch 2 becomes fully live (receipt published, command submitted), and the run loop
    // drains commands before each formation, so the next re-formed span covers both batches
    // and its attempt succeeds.
    let batch2 = harness.scheduler.schedule(block(2, 1), vec![lane_tx()]);
    batch2.wait_committed_blocking();
    batch2.write_batch_receipt(batch_receipt_from(batch2.checkpoint().metadata())).wait_blocking();
    batch2.publish_artifact(Some(settlement_journal()));
    harness.prover.submit(&batch2);

    let bundle =
        next_bundle(&harness.queue, Duration::from_secs(10)).expect("the re-formed bundle");
    bundle.wait_artifact_published_blocking();
    assert_eq!(
        bundle.block_prove_to(),
        block_hash(2),
        "the retry spans the batch that arrived during the failures",
    );
    harness.prover.shutdown();
    harness.scheduler.shutdown();
}

/// With nothing new arriving, a failed attempt retries the identical span (any retry fixes
/// the random CUDA invalidity) and emits exactly one bundle for it.
#[test]
fn a_failed_attempt_retries_the_same_span_when_nothing_arrived() {
    let harness = setup(true, 1);
    wait_until("first failed attempt", Duration::from_secs(10), || {
        harness.backend.attempts.load(Ordering::SeqCst) >= 1
    });
    harness.backend.failing.store(false, Ordering::SeqCst);

    let bundle = next_bundle(&harness.queue, Duration::from_secs(10)).expect("the retried bundle");
    bundle.wait_artifact_published_blocking();
    assert_eq!(bundle.block_prove_to(), block_hash(1), "the same span is retried");
    assert!(
        next_bundle(&harness.queue, Duration::from_millis(500)).is_none(),
        "the span emits exactly one bundle",
    );
    harness.prover.shutdown();
    harness.scheduler.shutdown();
}
