//! Pins the re-form of a retained suffix that survives a settlement the prover observes:
//! a boundary matching window batches drains them and re-proves the remainder (the supersede
//! convergence path), and a boundary matching nothing must still fall through to that re-form
//! instead of returning early, but only for a front the settler journal no longer names. The
//! early return stranded the suffix whenever no later settlement ever matched again: the watch
//! republishes the last settlement per block, so once that boundary's batches are drained
//! every subsequent wake matched nothing and the surviving batches (and their exit leaves)
//! waited for unrelated lane activity to be bundled again. The journal gate keeps the
//! fall-through honest in both directions: a front whose bundle still sits in the settlement
//! queue (the queue path's own emission) is left alone, while a front the settler dropped
//! (its entry deleted on the supersede skip, or a drain's mid-bundle start that no entry ever
//! named) re-forms exactly once.

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
use tokio::sync::watch;
use vprogs_core_atomics::AsyncQueue;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId, SchedulerTransaction};
use vprogs_l1_types::{ChainBlockMetadata, SettlementInfo, TransactionId};
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler, TransactionContext};
use vprogs_state_settlement_journal::StoreJournal;
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

/// Backend standing in for all three guests: the aggregator receipt is the settlement journal
/// itself (identity `journal_bytes`), so the worker parses exactly the transition
/// [`settlement_journal`] encodes.
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

/// The on-chain settlement of a bundle proving through `block`, keyed to a distinct tx per
/// block: each phase republishes a settlement the worker has already acted on once, and a
/// shared tx would let the settlement dedupe swallow the later pass.
fn tip_through(block: u8) -> SettlementInfo {
    SettlementInfo {
        block_prove_to: block_hash(block),
        tx_id: TransactionId::from_bytes([block; 32]),
        ..Default::default()
    }
}

/// Harness holding the live pieces a re-form test drives, plus the temp dir backing its store
/// and the settlement journal the worker records into.
struct Harness {
    _temp: TempDir,
    scheduler: Scheduler<RocksDbStore, PlainProcessor>,
    prover: AggregateProver<RocksDbStore, PlainProcessor>,
    queue: AsyncQueue<ScheduledBundle<SettlementArtifact<Vec<u8>>>>,
    settlement_tx: watch::Sender<Option<SettlementInfo>>,
    journal: StoreJournal<RocksDbStore>,
}

/// Builds the harness and commits one batch per `blocks` entry. With `one_bundle` the batches
/// land in a single bundle spanning every block; otherwise each batch bundles on its own,
/// leaving one journal entry per batch. Every batch is proved and retained when the test's
/// settlement arrives, and the bundle cap leaves headroom for a re-formed suffix to span
/// every batch at once.
fn harness(blocks: &[(u8, u64)], one_bundle: bool) -> Harness {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
    let journal = StoreJournal::new(storage.clone());
    let mut scheduler = Scheduler::new(
        ExecutionConfig::default().with_processor(PlainProcessor),
        StorageConfig::default().with_store(storage),
    );
    let queue: AsyncQueue<ScheduledBundle<SettlementArtifact<Vec<u8>>>> = AsyncQueue::new();
    let (settlement_tx, settlement_rx) = watch::channel::<Option<SettlementInfo>>(None);
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
            re_root: None,
            bundle_size: 1..=8,
            exits: None,
            tip_daa: None,
            follow_depth: 0,
        },
    );

    // Boot determinism: the committed-gap gate must see an empty journal before the first batch
    // commits. A worker that boots after a commit treats the whole committed span as a restart
    // gap and re-feeds it, which would emit duplicate bundles unrelated to the behavior under
    // test.
    prover.wait_booted();

    if one_bundle {
        // Submit every batch while the worker parks on the first one's artifact, then publish
        // the artifacts front-last: the parked wake fires only when the front publishes, and by
        // then every follower's artifact is already out, so the greedy extend spans them all.
        let mut batches = Vec::new();
        for &(hash, parent) in blocks {
            let batch = scheduler.schedule(block(hash, parent), vec![lane_tx()]);
            batch.wait_committed_blocking();
            batch.write_batch_receipt(settlement_journal()).wait_blocking();
            batches.push(batch);
        }
        for batch in batches.iter().skip(1).rev() {
            batch.publish_artifact(Some(settlement_journal()));
        }
        for batch in &batches {
            prover.submit(batch);
        }
        batches[0].publish_artifact(Some(settlement_journal()));
        let bundle = next_bundle(&queue, Duration::from_secs(10)).expect("the batches must bundle");
        bundle.wait_artifact_published_blocking();
        assert_eq!(bundle.block_prove_to(), block_hash(blocks.last().unwrap().0));
    } else {
        for &(hash, parent) in blocks {
            let batch = scheduler.schedule(block(hash, parent), vec![lane_tx()]);
            batch.wait_committed_blocking();
            batch.write_batch_receipt(settlement_journal()).wait_blocking();
            batch.publish_artifact(Some(settlement_journal()));
            prover.submit(&batch);
            let bundle =
                next_bundle(&queue, Duration::from_secs(10)).expect("the batch must bundle");
            bundle.wait_artifact_published_blocking();
            assert_eq!(bundle.block_prove_to(), block_hash(hash));
        }
    }

    Harness { _temp: temp_dir, scheduler, prover, queue, settlement_tx, journal }
}

/// Tests that a settlement boundary draining into a formed bundle re-proves the surviving
/// mid-bundle suffix (a front no journal entry names, so nothing is pending for it), and that
/// republishing the same settlement (the watch's per-block cadence) emits no duplicate.
#[test]
fn matched_boundary_drains_and_reforms_the_suffix() {
    let harness = harness(&[(1, 0), (2, 1)], true);

    // A settlement through block 1 drains the bundle's first batch: the surviving front starts
    // mid-bundle, no journal entry names it, and the suffix must re-form.
    harness.settlement_tx.send_replace(Some(tip_through(1)));
    let reformed =
        next_bundle(&harness.queue, Duration::from_secs(10)).expect("the suffix must re-form");
    reformed.wait_artifact_published_blocking();
    assert_eq!(reformed.block_prove_to(), block_hash(2));

    // The per-block republish of the same tip must not re-form the same suffix again.
    harness.settlement_tx.send_replace(Some(tip_through(1)));
    assert!(
        next_bundle(&harness.queue, Duration::from_millis(500)).is_none(),
        "the re-formed suffix must not be re-proved under the republished tip",
    );

    harness.prover.shutdown();
    harness.scheduler.shutdown();
}

/// Tests both sides of the unmatched-boundary gate: a boundary matching no window batch leaves
/// a front whose journal entry still exists (its bundle pending in the settlement queue)
/// untouched, and the same front re-forms exactly once once the settler's supersede skip
/// deletes that entry, instead of waiting for unrelated lane activity.
#[test]
fn unmatched_boundary_reforms_only_a_front_the_settler_dropped() {
    let harness = harness(&[(1, 0), (2, 1)], false);

    // A settlement on a block no test batch carries: nothing drains, and the front's bundle is
    // still pending in the settlement queue, so the fall-through must not re-prove it.
    harness.settlement_tx.send_replace(Some(tip_through(0xff)));
    assert!(
        next_bundle(&harness.queue, Duration::from_millis(500)).is_none(),
        "a pending front must not be re-proved under an unmatched boundary",
    );

    // The settler's supersede skip resolves the front's bundle on chain and deletes its entry:
    // the front is now stranded, and the next settlement (a fresh tx, so the dedupe lets it
    // through) must re-form it.
    let front = harness
        .journal
        .entries()
        .first()
        .map(|(start, _)| *start)
        .expect("the queue path journaled the front");
    harness.journal.delete(front);
    harness.settlement_tx.send_replace(Some(tip_through(0xfe)));
    let reformed = next_bundle(&harness.queue, Duration::from_secs(10))
        .expect("the stranded suffix must re-form");
    reformed.wait_artifact_published_blocking();
    assert_eq!(reformed.block_prove_to(), block_hash(2));

    // The republish is idempotent: the re-form re-journaled the front, pinning the suffix
    // until something drains it.
    harness.settlement_tx.send_replace(Some(tip_through(0xfe)));
    assert!(
        next_bundle(&harness.queue, Duration::from_millis(500)).is_none(),
        "the re-formed suffix must not be re-proved under the republished tip",
    );

    harness.prover.shutdown();
    harness.scheduler.shutdown();
}
