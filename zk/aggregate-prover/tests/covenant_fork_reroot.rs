//! Pins the covenant re-root: a receipt-less committed range above the settled boundary (or a
//! fork below it) leaves nothing chaining from the covenant tip, and the first batch the
//! windows hold enters off the tip's exit. The worker used to fold those batches anyway: each
//! bundle proved from a base the covenant never took, the settler skipped it, no settlement
//! ever advanced, and the only re-rooting trigger (the settlement-watch advance) never fired.
//! The live Oct 25 2026 wedge. The fold must instead hold while the journal holds nothing
//! chaining from the tip, and request a rollback of execution to the covenant boundary so
//! re-execution re-enters at the covenant's exit.

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
use tokio::sync::{mpsc, watch};
use vprogs_core_atomics::AsyncQueue;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId, SchedulerTransaction};
use vprogs_l1_types::{ChainBlockMetadata, SettlementInfo};
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler, TransactionContext};
use vprogs_state_settlement_journal::StoreJournal;
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_abi::batch_aggregator::{StateTransition, StateTransitionArgs};
use vprogs_zk_aggregate_prover::{
    AggregateProver, AggregateProverConfig, ScheduledBundle, SettlementArtifact,
};
use vprogs_zk_batch_prover::{LaneProofError, LaneProofRequest, LaneProofSource};

const TX_IMAGE_ID: [u8; 32] = [0u8; 32];
const BATCH_IMAGE_ID: [u8; 32] = [1u8; 32];
const AGGREGATOR_IMAGE_ID: [u8; 32] = [2u8; 32];

fn seq_commit() -> Hash {
    Hash::from_bytes([0x33; 32])
}

fn block_hash(byte: u8) -> Hash {
    Hash::from_bytes([byte; 32])
}

/// Chain-block metadata carrying an explicit lane chain, so a fork's batches enter at a lane
/// tip the covenant never took (all-distinct from the covenant's exit).
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

fn lane_tx() -> SchedulerTransaction<usize> {
    SchedulerTransaction::new(0, vec![AccessMetadata::write(ResourceId::for_test(1))], 0)
}

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

struct Harness {
    _temp: TempDir,
    scheduler: Scheduler<RocksDbStore, PlainProcessor>,
    prover: AggregateProver<RocksDbStore, PlainProcessor>,
    queue: AsyncQueue<ScheduledBundle<SettlementArtifact<Vec<u8>>>>,
    settlement_tx: watch::Sender<Option<SettlementInfo>>,
    re_root_rx: mpsc::UnboundedReceiver<SettlementInfo>,
    journal: StoreJournal<RocksDbStore>,
}

fn harness() -> Harness {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
    let journal = StoreJournal::new(storage.clone());
    let scheduler = Scheduler::new(
        ExecutionConfig::default().with_processor(PlainProcessor),
        StorageConfig::default().with_store(storage),
    );
    let queue: AsyncQueue<ScheduledBundle<SettlementArtifact<Vec<u8>>>> = AsyncQueue::new();
    let (settlement_tx, settlement_rx) = watch::channel::<Option<SettlementInfo>>(None);
    let (re_root_tx, re_root_rx) = mpsc::unbounded_channel();
    let prover = AggregateProver::new(
        SyntheticBackend,
        scheduler.state().receipt_store(),
        Some(journal.clone()),
        AggregateProverConfig {
            lane_key: Hash::default(),
            covenant_id: None,
            lane_source: ServeLaneProofs,
            settlement_queue: Some(queue.clone()),
            settlement: Some(settlement_rx),
            re_root: Some(re_root_tx),
            bundle_size: 1..=8,
            exits: None,
        },
    );

    // Boot determinism: let the worker's committed-gap gate pass before the first commit.
    thread::sleep(Duration::from_millis(300));

    Harness { _temp: temp_dir, scheduler, prover, queue, settlement_tx, re_root_rx, journal }
}

fn commit_and_submit(
    scheduler: &mut Scheduler<RocksDbStore, PlainProcessor>,
    prover: &AggregateProver<RocksDbStore, PlainProcessor>,
    metadata: ChainBlockMetadata,
) {
    let batch = scheduler.schedule(metadata, vec![lane_tx()]);
    batch.wait_committed_blocking();
    batch.write_batch_receipt(settlement_journal()).wait_blocking();
    batch.publish_artifact(Some(settlement_journal()));
    prover.submit(&batch);
}

/// The settlement of the covenant tip: proves through the given block and leaves the lane at its
/// exit.
fn tip_through(hash: u8) -> SettlementInfo {
    SettlementInfo {
        block_prove_to: block_hash(hash),
        new_lane_tip: block_hash(hash),
        ..Default::default()
    }
}

#[test]
fn fork_below_the_settlement_requests_a_reroot_instead_of_folding_unlandable_batches() {
    let mut harness = harness();

    // The settled chain: batches 1..=3 over lane tips L1 -> L2 -> L3, one bundle each. Each
    // bundle is drained before the next block commits, so the fold window never holds two
    // committed batches (`bundle_size` allows folding them into one bundle, which would leave
    // no bundle proving to the middle block).
    let mut prev_lane_tip = Hash::default();
    for (id, parent_id) in [(1u8, 0u64), (2, 1), (3, 2)] {
        commit_and_submit(
            &mut harness.scheduler,
            &harness.prover,
            block(id, parent_id, prev_lane_tip),
        );
        prev_lane_tip = block_hash(id);

        let bundle = next_bundle(&harness.queue, Duration::from_secs(10)).expect("bundle forms");
        bundle.wait_artifact_published_blocking();
        assert_eq!(bundle.block_prove_to(), block_hash(id));
    }

    // The covenant settles through block 3: the windows drain on the advance.
    harness.settlement_tx.send_replace(Some(tip_through(3)));

    // The live Oct 25 shape: a receipt-less committed range above the boundary never bundles,
    // so the first batch the windows hold enters off the covenant's exit (its predecessor is a
    // later lane tip, not the settlement's). Folding it would publish a bundle starting
    // mid-chain; the settler can only skip it, and with no settlement advance to re-trigger
    // the drain the node wedges. The worker must hold the fold and request a re-root instead.
    commit_and_submit(&mut harness.scheduler, &harness.prover, block(4, 3, block_hash(0x99)));
    assert!(
        next_bundle(&harness.queue, Duration::from_millis(750)).is_none(),
        "batches entering off the covenant tip must not fold into a bundle",
    );
    let tip = harness
        .re_root_rx
        .try_recv()
        .expect("the refused fold must request a rollback to the covenant boundary");
    assert_eq!(tip.block_prove_to, block_hash(3));
    assert_eq!(tip.new_lane_tip, block_hash(3));

    // The runner-side rollback lands (the test stands in for it): execution rolls back to the
    // boundary the tip names, and the prover's windows drop the off-tip batch.
    harness.scheduler.rollback_to(3).expect("rollback");
    harness.prover.rollback(3);

    // Re-execution re-enters at the covenant's exit: the next batch folds and its bundle chains
    // from the tip.
    commit_and_submit(&mut harness.scheduler, &harness.prover, block(5, 3, block_hash(3)));
    let bundle = next_bundle(&harness.queue, Duration::from_secs(10)).expect("bundle forms");
    bundle.wait_artifact_published_blocking();
    assert_eq!(bundle.block_prove_to(), block_hash(5));

    harness.prover.shutdown();
    harness.scheduler.shutdown();
}

/// A covenant tip whose boundary sits below the bundling front while our own chain still
/// chains off the boundary's exit is covered catch-up (bundling lags a covenant chain behind
/// the pending work, under contention against a competitor's shorter settlement), not a wedge:
/// the fold must proceed and no re-root may fire. Pinning the settler's supersede skip with a
/// journal delete leaves the front off the tip's exit with nothing journaled, the exact shape
/// the coverage gate must release.
#[test]
fn boundary_below_a_chaining_front_keeps_bundling() {
    let mut harness = harness();

    // Batches 1-3 chain; each bundles on its own (the cap leaves the pair shape out).
    let mut prev_lane_tip = Hash::default();
    for (id, parent_id) in [(1u8, 0u64), (2, 1), (3, 2)] {
        commit_and_submit(
            &mut harness.scheduler,
            &harness.prover,
            block(id, parent_id, prev_lane_tip),
        );
        prev_lane_tip = block_hash(id);
        let bundle = next_bundle(&harness.queue, Duration::from_secs(10)).expect("bundle forms");
        bundle.wait_artifact_published_blocking();
    }

    // The covenant settles through block 2: the drain empties the windows below it and the
    // compaction deletes the covered entries, leaving only bundle 3's.
    harness.settlement_tx.send_replace(Some(tip_through(2)));
    let deadline = Instant::now() + Duration::from_secs(10);
    while harness.journal.entries().len() != 1 {
        assert!(Instant::now() < deadline, "the covered entries must compact");
        thread::sleep(Duration::from_millis(20));
    }

    // The settler's supersede skip resolves bundle 3 on chain and deletes its entry: the
    // journal is empty while the next batch still enters above the tip's exit.
    let pending = harness.journal.entries()[0].0;
    harness.journal.delete(pending);

    // Batch 4 chains off batch 3 (off the tip's older exit, as any lagging front does): it
    // must still fold, and the off-tip entry must not request a rollback.
    commit_and_submit(&mut harness.scheduler, &harness.prover, block(4, 3, block_hash(3)));
    let bundle = next_bundle(&harness.queue, Duration::from_secs(10))
        .expect("a chaining front must keep bundling");
    bundle.wait_artifact_published_blocking();
    assert_eq!(bundle.block_prove_to(), block_hash(4));
    assert!(
        harness.re_root_rx.try_recv().is_err(),
        "a boundary below a chaining front must not request a re-root",
    );

    harness.prover.shutdown();
    harness.scheduler.shutdown();
}

/// A covenant tip proving through a block past everything we executed (a competitor settled
/// ahead of our tip) leaves the chaining front off the tip's exit with nothing to map the
/// boundary to: the fold must proceed, since the runner has no boundary of ours to roll back to
/// and holding would park bundling until the front reaches the covenant's chain.
#[test]
fn boundary_ahead_of_execution_keeps_bundling() {
    let mut harness = harness();

    let mut prev_lane_tip = Hash::default();
    for (id, parent_id) in [(1u8, 0u64), (2, 1)] {
        commit_and_submit(
            &mut harness.scheduler,
            &harness.prover,
            block(id, parent_id, prev_lane_tip),
        );
        prev_lane_tip = block_hash(id);
        let bundle = next_bundle(&harness.queue, Duration::from_secs(10)).expect("bundle forms");
        bundle.wait_artifact_published_blocking();
    }

    // The covenant settles through block 2 (draining and compacting both bundles), then
    // through a block we never executed: batch 3 still enters at the settled exit.
    harness.settlement_tx.send_replace(Some(tip_through(2)));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !harness.journal.entries().is_empty() {
        assert!(Instant::now() < deadline, "the covered entries must compact");
        thread::sleep(Duration::from_millis(20));
    }
    harness.settlement_tx.send_replace(Some(tip_through(9)));

    commit_and_submit(&mut harness.scheduler, &harness.prover, block(3, 2, block_hash(2)));
    let bundle = next_bundle(&harness.queue, Duration::from_secs(10))
        .expect("a front below an ahead boundary must keep bundling");
    bundle.wait_artifact_published_blocking();
    assert_eq!(bundle.block_prove_to(), block_hash(3));
    assert!(
        harness.re_root_rx.try_recv().is_err(),
        "a boundary mapping to no batch of ours must not request a re-root",
    );

    harness.prover.shutdown();
    harness.scheduler.shutdown();
}

/// Pins the same hold on the committed-gap re-cover: a committed batch whose cached receipt
/// enters off the covenant tip (a state fork below the boundary surviving in the receipts) must
/// not be re-covered into a bundle; the re-cover is held and the rollback requested, leaving the
/// journal clean instead of re-poisoning it with an unlandable entry. The live Oct 25 wedge
/// reached the settler through exactly this path after the resume pass had dropped the dead
/// entries.
#[test]
fn off_tip_gap_recover_holds_and_requests_reroot() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
    let journal = StoreJournal::new(storage.clone());
    let mut scheduler = Scheduler::new(
        ExecutionConfig::default().with_processor(PlainProcessor),
        StorageConfig::default().with_store(storage),
    );

    // A committed batch with its receipt cached but no journal entry, entering at the synthetic
    // default pins (state 0x00, zero lane tip): committed-but-unjournaled, the gap shape.
    let batch = scheduler.schedule(block(1, 0, Hash::default()), vec![lane_tx()]);
    batch.wait_committed_blocking();
    batch.write_batch_receipt(settlement_journal()).wait_blocking();
    batch.publish_artifact(Some(settlement_journal()));

    let queue: AsyncQueue<ScheduledBundle<SettlementArtifact<Vec<u8>>>> = AsyncQueue::new();
    let (settlement_tx, settlement_rx) = watch::channel::<Option<SettlementInfo>>(None);
    let (re_root_tx, mut re_root_rx) = mpsc::unbounded_channel();
    let prover = AggregateProver::new(
        SyntheticBackend,
        scheduler.state().receipt_store(),
        Some(journal.clone()),
        AggregateProverConfig {
            lane_key: Hash::default(),
            covenant_id: None,
            lane_source: ServeLaneProofs,
            settlement_queue: Some(queue.clone()),
            settlement: Some(settlement_rx),
            re_root: Some(re_root_tx),
            bundle_size: 1..=1,
            exits: None,
        },
    );

    // The covenant tip exited at pins the cached receipt does not enter at.
    settlement_tx.send_replace(Some(SettlementInfo {
        block_prove_to: block_hash(3),
        new_state: [0x11; 32],
        new_lane_tip: block_hash(3),
        ..Default::default()
    }));

    // No bundle may be composed from the off-tip receipts, and the rollback must be requested.
    assert!(
        next_bundle(&queue, Duration::from_millis(750)).is_none(),
        "an off-tip committed-gap re-cover must not reach the settlement queue",
    );
    let tip = re_root_rx
        .try_recv()
        .expect("the held re-cover must request a rollback to the covenant boundary");
    assert_eq!(tip.block_prove_to, block_hash(3));
    assert_eq!(tip.new_lane_tip, block_hash(3));
    assert!(journal.entries().is_empty(), "no unlandable entry may be journaled");

    prover.shutdown();
    scheduler.shutdown();
}

/// A receipt-less committed range above a boundary that resolves inside it is the same wedge
/// the mismatch hold names: the batches were killed mid-prove above a settlement the covenant
/// pinned, no cached artifact composes them, and only re-entering execution at the boundary
/// rebuilds their receipts. The re-cover requests the re-root instead of silently abandoning
/// the range to new-work bundling.
#[test]
fn killed_range_above_the_boundary_requests_reroot() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
    let journal = StoreJournal::new(storage.clone());
    let mut scheduler = Scheduler::new(
        ExecutionConfig::default().with_processor(PlainProcessor),
        StorageConfig::default().with_store(storage),
    );

    // The covenant's boundary batch, committed with its receipt: the boundary resolves to it.
    let boundary = scheduler.schedule(block(1, 0, Hash::default()), vec![lane_tx()]);
    boundary.wait_committed_blocking();
    boundary.write_batch_receipt(settlement_journal()).wait_blocking();
    boundary.publish_artifact(Some(settlement_journal()));

    // The killed range: committed past the boundary with no receipt cached (a kill between the
    // batch's commit and its prove).
    let killed = scheduler.schedule(block(2, 1, block_hash(1)), vec![lane_tx()]);
    killed.wait_committed_blocking();

    let queue: AsyncQueue<ScheduledBundle<SettlementArtifact<Vec<u8>>>> = AsyncQueue::new();
    let (settlement_tx, settlement_rx) = watch::channel::<Option<SettlementInfo>>(None);
    let (re_root_tx, mut re_root_rx) = mpsc::unbounded_channel();
    let prover = AggregateProver::new(
        SyntheticBackend,
        scheduler.state().receipt_store(),
        Some(journal.clone()),
        AggregateProverConfig {
            lane_key: Hash::default(),
            covenant_id: None,
            lane_source: ServeLaneProofs,
            settlement_queue: Some(queue.clone()),
            settlement: Some(settlement_rx),
            re_root: Some(re_root_tx),
            bundle_size: 1..=1,
            exits: None,
        },
    );

    // The covenant tip settled through the boundary batch: its block resolves inside the
    // committed range, above the (empty) journal tail.
    settlement_tx.send_replace(Some(SettlementInfo {
        block_prove_to: block_hash(1),
        new_state: [0x11; 32],
        new_lane_tip: block_hash(1),
        ..Default::default()
    }));

    assert!(
        next_bundle(&queue, Duration::from_millis(750)).is_none(),
        "a killed range above the boundary must not reach the settlement queue",
    );
    let tip = re_root_rx
        .try_recv()
        .expect("the uncoverable range must request a rollback to the covenant boundary");
    assert_eq!(tip.block_prove_to, block_hash(1));
    assert!(journal.entries().is_empty(), "no unlandable entry may be journaled");

    prover.shutdown();
    scheduler.shutdown();
}
