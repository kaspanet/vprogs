//! Pins the journal passes against a settlement boundary whose block hash no longer maps into
//! the batch metadata: the state a reorg leaves behind when it cancels the boundary batch
//! between its proof and its commit, so no metadata row ever pins the block.
//!
//! An unmapped boundary must still resolve through the journal's own record of the bundle that
//! settled (its entry ends at the boundary block), compacting the covered entries instead of
//! keeping them forever. Uncompacted, the settled range is re-proved into duplicate bundles the
//! settler must skip, and a restart re-feeds the covered entry as if it were still pending.

// The backend traits return `impl Future + 'static`, which an `async fn` cannot satisfy: its future
// borrows `&self`.
#![allow(clippy::manual_async_fn)]

use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use kaspa_hashes::Hash;
use kaspa_rpc_core::GetSeqCommitLaneProofResponse;
use tempfile::TempDir;
use tokio::sync::watch;
use vprogs_core_atomics::{AsyncQueue, AtomicAsyncLatch};
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId, SchedulerTransaction};
use vprogs_l1_types::{ChainBlockMetadata, SettlementInfo, TransactionId};
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler, TransactionContext};
use vprogs_state_proof_receipt::{AggregatorKey, Prefix, put as put_receipt};
use vprogs_state_settlement_journal::{JournalEntry, StoreJournal};
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::Store;
use vprogs_zk_abi::batch_aggregator::{StateTransition, StateTransitionArgs};
use vprogs_zk_aggregate_prover::{
    AggregateProver, AggregateProverConfig, ScheduledBundle, SettlementArtifact,
};
use vprogs_zk_batch_prover::{LaneProofError, LaneProofRequest, LaneProofSource};
use zerocopy::little_endian::U64;

/// Transaction payload whose execution parks until the test releases it.
const GATE_TX: usize = 100;

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

/// Processor that parks the execution of [`GATE_TX`] until `release` opens, keeping its batch
/// uncommitted (and therefore without a batch-metadata row) while the test drives the aggregate
/// prover: the state a reorg leaves when it cancels a batch between its proof and its commit.
/// `entered` opens only once `expected` gated transactions are parked, so a test gating several
/// batches can wait for all of them.
#[derive(Clone)]
struct GateProcessor {
    /// Opened by the processor once the gated transaction is inside execution.
    entered: Arc<AtomicAsyncLatch>,
    /// Opened by the test to let the gated transaction finish.
    release: Arc<AtomicAsyncLatch>,
    /// How many gated transactions must park before `entered` opens.
    expected: u64,
    /// Gated transactions parked so far.
    parked: Arc<AtomicU64>,
}

impl vprogs_scheduling_scheduler::Processor<RocksDbStore> for GateProcessor {
    fn process_transaction(
        &self,
        ctx: &mut TransactionContext<RocksDbStore, Self>,
    ) -> Result<(), Self::Error> {
        if ctx.scheduler_tx().tx == GATE_TX {
            if self.parked.fetch_add(1, Ordering::SeqCst) + 1 == self.expected {
                self.entered.open();
            }
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

/// A `GateProcessor` parking `expected` gated transactions before `entered` opens.
fn gate(expected: u64) -> (GateProcessor, Arc<AtomicAsyncLatch>, Arc<AtomicAsyncLatch>) {
    let entered = Arc::new(AtomicAsyncLatch::new());
    let release = Arc::new(AtomicAsyncLatch::new());
    (
        GateProcessor {
            entered: entered.clone(),
            release: release.clone(),
            expected,
            parked: Arc::new(AtomicU64::new(0)),
        },
        entered,
        release,
    )
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

/// Spins until `journal` holds exactly `want` entries, so the test proceeds only after the
/// worker's pass has actually run.
fn wait_for_entries(journal: &StoreJournal<RocksDbStore>, want: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while journal.entries().len() != want {
        assert!(
            Instant::now() < deadline,
            "the journal never reached {want} entries (has {})",
            journal.entries().len(),
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// The on-chain settlement of the bundle over `block`: it proves through that block, whose
/// metadata row never existed.
fn tip_through(block: u8) -> SettlementInfo {
    SettlementInfo { block_prove_to: block_hash(block), ..Default::default() }
}

/// Tests that a settlement boundary mapping to no batch metadata (the reorg canceled the
/// boundary batch between its proof and its commit, so no row pins the block) still compacts the
/// journal entry of the bundle that settled: the journal's own record ends at the boundary block.
#[test]
fn unmapped_boundary_still_compacts_the_journal() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let (processor, entered, release) = gate(1);
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let journal = StoreJournal::new(storage.clone());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(processor),
            StorageConfig::default().with_store(storage),
        );

        // The bundle's only batch parks inside execution: proved and journaled by the worker,
        // but never committed, so its block gains no metadata row.
        let gated = scheduler.schedule(
            block(1, 0),
            vec![SchedulerTransaction::new(
                0,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                GATE_TX,
            )],
        );
        entered.wait_blocking();

        let settlement_queue: AsyncQueue<ScheduledBundle<SettlementArtifact<Vec<u8>>>> =
            AsyncQueue::new();
        let (settlement_tx, settlement_rx) = watch::channel::<Option<SettlementInfo>>(None);
        let prover = AggregateProver::new(
            SyntheticBackend,
            scheduler.state().receipt_store(),
            Some(journal.clone()),
            AggregateProverConfig {
                lane_key: Hash::default(),
                covenant_id: None,
                lane_source: ServeLaneProofs,
                settlement_queue: Some(settlement_queue.clone()),
                settlement: Some(settlement_rx),
                bundle_size: 1..=1,
                exits: None,
            },
        );

        // The batch's receipt and artifact exist (a proof raced ahead of the reorg), so the
        // worker bundles and journals it although the scheduler never commits it.
        gated.write_batch_receipt(settlement_journal()).wait_blocking();
        gated.publish_artifact(Some(settlement_journal()));
        prover.submit(&gated);
        let bundle = next_bundle(&settlement_queue, Duration::from_secs(10))
            .expect("the gated batch must bundle once its receipt is published");
        bundle.wait_artifact_published_blocking();
        assert_eq!(bundle.block_prove_to(), block_hash(1));
        wait_for_entries(&journal, 1);

        // The bundle's settlement lands on chain and reaches the watch while the worker is
        // parked. Its boundary block has no metadata row, so the block-hash lookup misses; the
        // covered entry must still go.
        settlement_tx.send_replace(Some(tip_through(1)));
        wait_for_entries(&journal, 0);

        // Nothing re-forms over the compacted range.
        assert!(
            next_bundle(&settlement_queue, Duration::from_millis(500)).is_none(),
            "no duplicate bundle may be emitted over the settled range",
        );

        release.open();
        prover.shutdown();
        scheduler.shutdown();
    }
}

/// Tests that a restart facing the same unmapped boundary does not re-feed the covered entry as
/// pending work: the resumed worker resolves the boundary through the journal entry and deletes
/// it, instead of re-feeding the settled bundle onto the settlement queue.
#[test]
fn unmapped_boundary_resume_does_not_refeed_the_settled_entry() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        // The pre-restart half, forged directly: the journal holds the proved bundle's entry over
        // a batch that never committed (no metadata row), and its aggregate receipt is cached.
        {
            let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
            let journal = StoreJournal::new(storage.clone());
            journal.record(
                1,
                &JournalEntry {
                    end_index: 1,
                    from_block: block_hash(1),
                    block_prove_to: block_hash(1),
                    seq_commit: seq_commit(),
                },
            );
            let agg_key = AggregatorKey {
                prefix: Prefix { checkpoint_index: 1.into() },
                block_hash: block_hash(1).as_bytes(),
                image_id: AGGREGATOR_IMAGE_ID,
                seq_commit: seq_commit().as_bytes(),
            };
            let mut wb = storage.write_batch();
            put_receipt(&mut wb, &agg_key, &settlement_journal());
            storage.commit(wb);
        }

        // The restarted worker: the journal holds an entry, so the startup gate waits for the
        // bridge's tip publication; the boundary it publishes maps to no batch metadata.
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let journal = StoreJournal::new(storage.clone());
        let scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(NoopProcessor),
            StorageConfig::default().with_store(storage),
        );
        let settlement_queue: AsyncQueue<ScheduledBundle<SettlementArtifact<Vec<u8>>>> =
            AsyncQueue::new();
        let (settlement_tx, settlement_rx) = watch::channel::<Option<SettlementInfo>>(None);
        let prover = AggregateProver::new(
            SyntheticBackend,
            scheduler.state().receipt_store(),
            Some(journal.clone()),
            AggregateProverConfig {
                lane_key: Hash::default(),
                covenant_id: None,
                lane_source: ServeLaneProofs,
                settlement_queue: Some(settlement_queue.clone()),
                settlement: Some(settlement_rx),
                bundle_size: 1..=1,
                exits: None,
            },
        );
        settlement_tx.send_replace(Some(tip_through(1)));
        wait_for_entries(&journal, 0);

        // The settled entry is gone and nothing re-feeds it as pending work.
        assert!(
            next_bundle(&settlement_queue, Duration::from_millis(500)).is_none(),
            "the settled entry must not be re-fed onto the settlement queue",
        );

        prover.shutdown();
        scheduler.shutdown();
    }
}

/// Chain-block metadata whose lane tip chains off `prev_tip`: the geometry every scheduled lane
/// has, where batch `n + 1` enters at batch `n`'s exit tip.
fn chained_block(hash: u8, prev_tip: Hash) -> ChainBlockMetadata {
    ChainBlockMetadata {
        hash: block_hash(hash),
        prev_lane_tip: prev_tip,
        lane_tip: block_hash(hash),
        seq_commit: seq_commit(),
        ..Default::default()
    }
}

/// Tests that a settlement boundary mapping to no window block, no batch metadata row, and no
/// journal entry still drains the covered prefix: the windows keep only batches chaining off the
/// settlement's exit tip (`prev_lane_tip == new_lane_tip`), walking `retained` and `queued` as
/// one chain so a match inside `retained` never consumes the queued work above it, and journal
/// entries entirely below the surviving front are deleted. This is the state a reorg leaves when
/// it replaces the boundary batch's block after the covering entry already went, the recurrence
/// observed live as an ever-growing journal ("keeping N entries", one more per bundle) with every
/// new bundle skipped by the settler as a stale-base duplicate.
#[test]
fn unmapped_boundary_drains_the_covered_prefix_by_lane_tip() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        // Every batch carries a gated transaction, so each is parked in execution or waiting for
        // an executor slot: none commits while the test drives the worker, and the startup
        // committed-gap pass must see no committed batch to cover, or it would race the original
        // bundles with a re-formed gap bundle. Waiting for one park (not five) keeps the test
        // independent of the executor pool width: the scheduler sizes it by physical CPU count,
        // and a pool narrower than five would leave the fifth batch queued and this latch shut.
        let (processor, entered, release) = gate(1);
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let journal = StoreJournal::new(storage.clone());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(processor),
            StorageConfig::default().with_store(storage),
        );

        // Four batches whose lane tips chain. Batches 1-3 carry published artifacts; batch 4's
        // stays unpublished until the test releases it, so after the first bundle ([1..2], the
        // size cap) the worker parks with batch 3 queued: `retained` holds batches 1-2 and
        // `queued` holds 3, the split the drain must walk as one chain.
        let batches: Vec<_> = [
            (1u8, Hash::default()),
            (2, block_hash(1)),
            (3, block_hash(2)),
            (4, block_hash(3)),
            (5, block_hash(4)),
        ]
        .into_iter()
        .map(|(hash, prev_tip)| {
            scheduler.schedule(
                chained_block(hash, prev_tip),
                vec![SchedulerTransaction::new(
                    0,
                    vec![AccessMetadata::write(ResourceId::for_test(hash.into()))],
                    GATE_TX,
                )],
            )
        })
        .collect();
        entered.wait_blocking();

        let settlement_queue: AsyncQueue<ScheduledBundle<SettlementArtifact<Vec<u8>>>> =
            AsyncQueue::new();
        let (settlement_tx, settlement_rx) = watch::channel::<Option<SettlementInfo>>(None);
        let prover = AggregateProver::new(
            SyntheticBackend,
            scheduler.state().receipt_store(),
            Some(journal.clone()),
            AggregateProverConfig {
                lane_key: Hash::default(),
                covenant_id: None,
                lane_source: ServeLaneProofs,
                settlement_queue: Some(settlement_queue.clone()),
                settlement: Some(settlement_rx),
                bundle_size: 2..=2,
                exits: None,
            },
        );

        for batch in &batches[..3] {
            prover.submit(batch);
            batch.write_batch_receipt(settlement_journal()).wait_blocking();
            batch.publish_artifact(Some(settlement_journal()));
        }
        wait_for_entries(&journal, 1);
        let bundle = next_bundle(&settlement_queue, Duration::from_secs(10))
            .expect("the published pair must bundle");
        bundle.wait_artifact_published_blocking();
        assert_eq!(bundle.checkpoint_index(), 1);

        // The settlement proves through a block no window batch, metadata row, or journal entry
        // names, and covers only batch 1: its exit tip is batch 1's. The drain stops inside
        // `retained` (batch 2 chains off the tip), so the queued batch 3 must survive it.
        let tip = SettlementInfo {
            tx_id: TransactionId::from([0xaa; 32]),
            block_prove_to: block_hash(0xaa),
            new_lane_tip: block_hash(1),
            daa_score: U64::new(10),
            ..Default::default()
        };
        settlement_tx.send_replace(Some(tip));
        // The covered prefix is gone from the windows; the straddling original entry [1..2]
        // stays (it spans the surviving front), and the re-formed suffix [2..2] records its own.
        wait_for_entries(&journal, 2);
        let starts: Vec<u64> = journal.entries().into_iter().map(|(start, _)| start).collect();
        assert_eq!(starts, vec![1, 2], "the straddler and the re-formed entry remain");
        let reformed = next_bundle(&settlement_queue, Duration::from_secs(10))
            .expect("the surviving suffix must re-form");
        reformed.wait_artifact_published_blocking();
        assert_eq!(reformed.checkpoint_index(), 2);

        // Republishing the same settlement re-runs nothing: the journal is unchanged and no
        // further bundle is emitted.
        settlement_tx.send_replace(Some(tip));
        assert_eq!(journal.entries().len(), 2);
        assert!(
            next_bundle(&settlement_queue, Duration::from_millis(500)).is_none(),
            "a republished settlement may not re-emit a bundle",
        );

        // A distinct but older settlement (a reorg flip republishing one the chain already
        // passed, its containing block scoring below the drained one) drains nothing: no batch
        // chains off its exit tip, and dropping the windows on it would discard the unsettled
        // work above its range.
        settlement_tx.send_replace(Some(SettlementInfo {
            tx_id: TransactionId::from([0xad; 32]),
            block_prove_to: block_hash(0xad),
            new_lane_tip: Hash::default(),
            daa_score: U64::new(5),
            ..Default::default()
        }));
        assert_eq!(
            journal.entries().len(),
            2,
            "a settlement below the drained one must not compact or drop anything",
        );

        // The queued work above the covered prefix still bundles once its pair completes: an
        // independent per-window drain would have consumed batches 3-4 as if they were covered.
        prover.submit(&batches[3]);
        batches[3].write_batch_receipt(settlement_journal()).wait_blocking();
        batches[3].publish_artifact(Some(settlement_journal()));
        let bundle = next_bundle(&settlement_queue, Duration::from_secs(10))
            .expect("the queued batches above the covered prefix must still bundle");
        bundle.wait_artifact_published_blocking();
        assert_eq!(bundle.checkpoint_index(), 3);
        wait_for_entries(&journal, 3);

        // A final settlement through batch 4's exit tip covers everything, but no successor is
        // scheduled yet: nothing drains, because empty-of-successor windows cannot tell covered
        // batches from pending work above the tip (a restart's re-fed bundles live in the
        // journal, not the windows, and wiping it on this tip would drop them).
        settlement_tx.send_replace(Some(SettlementInfo {
            tx_id: TransactionId::from([0xab; 32]),
            block_prove_to: block_hash(0xab),
            new_lane_tip: block_hash(4),
            daa_score: U64::new(20),
            ..Default::default()
        }));
        // Give the worker a moment to (wrongly) act on it before asserting it did not: the
        // no-successor branch leaves no trace of its own when correct.
        thread::sleep(Duration::from_millis(500));
        assert_eq!(
            journal.entries().len(),
            3,
            "a settlement with no scheduled successor must not drain or wipe anything",
        );

        // The successor's arrival is what drains the residue: batch 5 chains off the settlement's
        // exit tip, so a settlement advance finds it and drops the covered prefix ahead of it.
        prover.submit(&batches[4]);
        batches[4].write_batch_receipt(settlement_journal()).wait_blocking();
        batches[4].publish_artifact(Some(settlement_journal()));
        settlement_tx.send_replace(Some(SettlementInfo {
            tx_id: TransactionId::from([0xae; 32]),
            block_prove_to: block_hash(0xae),
            new_lane_tip: block_hash(4),
            daa_score: U64::new(25),
            ..Default::default()
        }));
        wait_for_entries(&journal, 0);

        release.open();
        prover.shutdown();
        scheduler.shutdown();
    }
}

/// Processor executing every transaction without touching resource bytes.
#[derive(Clone)]
struct NoopProcessor;

impl vprogs_scheduling_scheduler::Processor<RocksDbStore> for NoopProcessor {
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
