//! Pins the funding-race robustness of the committed-gap recovery: a settlement that dies
//! without landing leaves a journal entry chaining from a base the covenant never took, and the
//! resume pass must drop that dead entry so the committed-gap pass re-covers its range as one
//! fresh bundle chaining from the on-chain tip. The drop is decided only on a pass whose tip
//! transaction differs from the startup baseline (the bridge's pre-downtime publication, which
//! trails the chain by the follow depth while the settler advances on a probe confirm): a live
//! entry chaining from that still-unpublished settlement reads identically at the baseline and
//! must be re-fed, not dropped. New work arriving while that decision is deferred must wait for
//! it: a bundle journaled above the undecided entries would move the journal tail past the range
//! a later drop hands to the committed-gap pass, leaving it uncovered. The held work must resume
//! once the decision is made either way: a hold that lifted only on a drop would stall all new
//! proving behind a live entry. A committed-gap range whose final block's lane proof is
//! unobtainable (pruned lane history) must walk its end down to the live prefix instead of
//! deferring the same dead-ended range on every wake.

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
use vprogs_core_atomics::AsyncQueue;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId, SchedulerTransaction};
use vprogs_l1_types::{ChainBlockMetadata, SettlementInfo, TransactionId};
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler, SchedulerState, TransactionContext};
use vprogs_state_proof_receipt::{AggregatorKey, Prefix};
use vprogs_state_settlement_journal::{JournalEntry, StoreJournal};
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

/// Lane source serving every fetch, so a gap fails only on a missing receipt, never on a fetch.
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

/// Chain-block metadata for a block carrying the journal's `seq_commit` and an advancing lane tip,
/// so the committed-gap pass composes this batch's cached receipt.
fn block(hash: u8, parent_id: u64) -> ChainBlockMetadata {
    ChainBlockMetadata {
        hash: Hash::from_bytes([hash; 32]),
        parent_id,
        seq_commit: seq_commit(),
        prev_lane_tip: Hash::from_bytes([hash.saturating_sub(1); 32]),
        lane_tip: Hash::from_bytes([hash; 32]),
        ..Default::default()
    }
}

/// Encodes the per-batch receipt journal for the block built from `hash`: the lane pins match
/// the block metadata ([`block`] chains each block's lane tip onto the previous block's) and
/// the state pins are flat, so adjacent receipts chain.
fn batch_receipt(hash: u8) -> Vec<u8> {
    batch_receipt_entering(hash, &[0x11; 32], &Hash::from_bytes([hash.saturating_sub(1); 32]))
}

/// [`batch_receipt`] with explicit entry pins, for the first gap batch chaining off the tip.
fn batch_receipt_entering(hash: u8, prev_state: &[u8; 32], prev_lane_tip: &Hash) -> Vec<u8> {
    let mut buf = Vec::new();
    BatchTransition::encode(
        &mut buf,
        BatchTransitionArgs {
            prev_state,
            prev_lane_tip,
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

/// Commits a one-transaction batch for `meta` and seeds its cached per-batch receipt the way the
/// batch prover would have, returning the batch handle without publishing or submitting it: the
/// caller decides when the pipeline sees this batch as live work.
fn commit_batch_with_receipt(
    scheduler: &mut Scheduler<RocksDbStore, PlainProcessor>,
    meta: ChainBlockMetadata,
) -> vprogs_scheduling_scheduler::ScheduledBatch<RocksDbStore, PlainProcessor> {
    let batch = scheduler.schedule(meta, vec![lane_tx()]);
    batch.wait_committed_blocking();
    batch.write_batch_receipt(batch_receipt(meta.hash.as_bytes()[0])).wait_blocking();
    batch
}

/// [`commit_batch_with_receipt`] with the first gap batch's receipt entering at the tip's pins.
fn commit_tip_chained_batch(
    scheduler: &mut Scheduler<RocksDbStore, PlainProcessor>,
    meta: ChainBlockMetadata,
) {
    let batch = scheduler.schedule(meta, vec![lane_tx()]);
    batch.wait_committed_blocking();
    batch.write_batch_receipt(tip_chained_batch_receipt(meta.hash.as_bytes()[0])).wait_blocking();
}

/// Receipt of the first gap batch, entering at the settled tip's own exit pins so the re-cover
/// composes from the tip: the synthetic tip's pins are the state/lane defaults.
fn tip_chained_batch_receipt(hash: u8) -> Vec<u8> {
    batch_receipt_entering(hash, &[0u8; 32], &Hash::default())
}

/// A settlement tip proving through `block`, so the resume pass resolves it against the journal.
fn tip_through(block: u8) -> SettlementInfo {
    SettlementInfo { block_prove_to: block_hash(block), ..Default::default() }
}

/// A settlement tip proving through `block` whose transaction id derives from the block byte
/// (so an advance past the baseline tip is identifiable) and which settles `new_state` (the pin
/// the resume's chaining check reads).
fn tip_with(block: u8, new_state: [u8; 32]) -> SettlementInfo {
    SettlementInfo {
        tx_id: TransactionId::from_bytes([block; 32]),
        block_prove_to: block_hash(block),
        new_state,
        ..Default::default()
    }
}

/// Encodes the settlement journal of a bundle that chains from a base at state `[0xAA]`
/// (whether that base is dead or merely unpublished is what the resume passes decide):
/// identical to [`settlement_journal`] except its proven `prev_state`, so the tip-pins check
/// rejects it against a tip that has not settled `[0xAA]`.
fn stale_settlement_journal() -> Vec<u8> {
    let mut buf = Vec::new();
    StateTransition::encode(
        &mut buf,
        StateTransitionArgs {
            prev_state: &[0xAA; 32],
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

/// Commits the pre-restart batches every resume test builds on and records their one journal
/// entry: block 1 settled, blocks 2 and 3 proved above it (the first entering at the settled
/// tip's pins, the way the re-cover composes), and the entry `3..=3` above the settled tip.
fn commit_dead_shape(
    scheduler: &mut Scheduler<RocksDbStore, PlainProcessor>,
    journal: &StoreJournal<RocksDbStore>,
) {
    commit_batch_with_receipt(scheduler, block(1, 0));
    commit_tip_chained_batch(scheduler, block(2, 1));
    commit_batch_with_receipt(scheduler, block(3, 2));
    journal.record(
        3,
        &JournalEntry {
            end_index: 3,
            from_block: block_hash(3),
            block_prove_to: block_hash(3),
            seq_commit: seq_commit(),
        },
    );
}

/// Stores the aggregate receipt of the entry [`commit_dead_shape`] records where a re-feed
/// reloads it.
///
/// The receipt chains from a base at state `[0xAA]` (dead, or our own unpublished settlement;
/// the resume passes decide).
fn store_stale_receipt(state: &SchedulerState<RocksDbStore, PlainProcessor>) {
    state
        .receipt_store()
        .write_agg_receipt(
            AggregatorKey {
                prefix: Prefix { checkpoint_index: 3.into() },
                block_hash: block_hash(3).as_bytes(),
                image_id: AGGREGATOR_IMAGE_ID,
                seq_commit: seq_commit().as_bytes(),
            },
            stale_settlement_journal(),
        )
        .wait_blocking();
}

/// The pre-restart shape of the resume tests that feed no new work: the committed batches and
/// journal entry of [`commit_dead_shape`], the entry's stale receipt, and the restart-side
/// scheduler state over the same store.
fn seeded_dead_shape(
    temp_dir: &TempDir,
) -> (StoreJournal<RocksDbStore>, SchedulerState<RocksDbStore, PlainProcessor>) {
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
    let journal = StoreJournal::new(storage.clone());
    let mut scheduler = Scheduler::new(
        ExecutionConfig::default().with_processor(PlainProcessor),
        StorageConfig::default().with_store(storage.clone()),
    );
    commit_dead_shape(&mut scheduler, &journal);
    scheduler.shutdown();

    // The restart half: the entry's aggregate receipt is stored where a re-feed would reload it.
    let state = SchedulerState::<RocksDbStore, PlainProcessor>::new(
        StorageConfig::default().with_store(storage.clone()),
    );
    store_stale_receipt(&state);
    (journal, state)
}

/// Tests that a journal entry left by a settlement that died without landing (its proven
/// transition chains from a base the covenant never took) survives the startup pass (whose
/// baseline tip may merely trail the chain) and is dropped by the first advance past the
/// baseline, after which the committed-gap pass re-covers the range as one fresh bundle chaining
/// from the on-chain tip. Re-feeding the dead entry forever instead wedges the settler: every
/// bundle re-folds from the stale base, the settler skips it as a base mismatch, and the entry
/// never resolves.
#[test]
fn committed_gap_with_dead_settlement_entry_recovers() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let (journal, state) = seeded_dead_shape(&temp_dir);

        let settlement_queue: AsyncQueue<ScheduledBundle<SettlementArtifact<Vec<u8>>>> =
            AsyncQueue::new();
        let (settlement_tx, settlement_rx) = watch::channel::<Option<SettlementInfo>>(None);
        let prover = AggregateProver::new(
            SyntheticBackend,
            state.receipt_store(),
            Some(journal.clone()),
            AggregateProverConfig {
                lane_key: Hash::default(),
                covenant_id: None,
                lane_source: ServeLaneProofs,
                settlement_queue: Some(settlement_queue.clone()),
                settlement: Some(settlement_rx),
                bundle_size: 1..=1,
                exits: None,
                re_root: None,
                tip_daa: None,
                follow_depth: 0,
            },
        );

        // The bridge's baseline tip proves through block 1 and carries the covenant's on-chain
        // state, which the entry's receipt does not chain from. The baseline may trail the
        // chain, so the startup pass defers the dead-entry decision and re-feeds the stored
        // receipt unchanged instead of dropping the entry.
        settlement_tx.send_replace(Some(tip_through(1)));
        let refeed = next_bundle(&settlement_queue, Duration::from_secs(10))
            .expect("the deferred startup re-feed");
        refeed.wait_artifact_published_blocking();
        assert_eq!(
            refeed.artifact().expect("the re-fed bundle carries its stored artifact").prev_state,
            [0xAA; 32],
            "the startup pass re-feeds the entry rather than deciding it against the baseline",
        );

        // The bridge publishes a settlement the baseline did not carry: the advance pass is
        // decisive, the entry still chains from nothing the tip carries, and it is dropped. The
        // committed-gap retry then re-covers the range as one fresh bundle chaining from the
        // tip. A republication re-feed of the fresh entry may follow the compose.
        settlement_tx.send_replace(Some(tip_with(1, [0x00; 32])));
        let mut prev_states = Vec::new();
        loop {
            let timeout = if prev_states.is_empty() {
                Duration::from_secs(10)
            } else {
                Duration::from_millis(500)
            };
            let Some(bundle) = next_bundle(&settlement_queue, timeout) else { break };
            bundle.wait_artifact_published_blocking();
            let artifact =
                bundle.artifact().expect("the re-covered bundle carries a real artifact");
            assert_eq!(
                bundle.block_prove_to(),
                block_hash(3),
                "the re-covered bundle spans the dead entry's whole range",
            );
            prev_states.push(artifact.prev_state);
            assert!(prev_states.len() <= 2, "at most the compose and one republication re-feed");
        }
        assert_eq!(
            prev_states.first(),
            Some(&[0x00; 32]),
            "the range is re-composed from the tip, not re-fed from the dead receipt",
        );

        let recorded: Vec<(u64, u64)> =
            journal.entries().into_iter().map(|(s, e)| (s, e.end_index)).collect();
        assert_eq!(recorded, vec![(2, 3)], "the re-covered entry replaces the dead one");

        prover.shutdown();
        state.storage().shutdown();
    }
}

/// Tests the wedge the deferred decision otherwise leaves on a lane only we settle: every
/// republication carries the baseline's transaction id (the settler skips every bundle chained
/// on the dead base, so no new settlement ever lands and no advance ever changes the id), so the
/// restamps themselves must decide. A restamp decides once the bridge tip has advanced the
/// follow depth past the startup baseline: by then every settlement confirmed before the crash
/// has surfaced under its own transaction id, so entries still chaining from a base the tip
/// does not carry are dead, the entry is dropped, and the committed-gap retry re-covers its
/// range from the tip. Restamps below the follow depth decide nothing.
#[test]
fn a_dead_entry_is_dropped_by_a_baseline_restamp_past_the_follow_depth() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let (journal, state) = seeded_dead_shape(&temp_dir);

        let settlement_queue: AsyncQueue<ScheduledBundle<SettlementArtifact<Vec<u8>>>> =
            AsyncQueue::new();
        let (settlement_tx, settlement_rx) = watch::channel::<Option<SettlementInfo>>(None);
        let tip_daa = Arc::new(AtomicU64::new(0));
        let prover = AggregateProver::new(
            SyntheticBackend,
            state.receipt_store(),
            Some(journal.clone()),
            AggregateProverConfig {
                lane_key: Hash::default(),
                covenant_id: None,
                lane_source: ServeLaneProofs,
                settlement_queue: Some(settlement_queue.clone()),
                settlement: Some(settlement_rx),
                bundle_size: 1..=1,
                exits: None,
                re_root: None,
                tip_daa: Some(tip_daa.clone()),
                follow_depth: 10,
            },
        );

        // The bridge's baseline tip proves through block 1 at daa 0; the startup pass defers the
        // dead-entry decision and re-feeds the stored receipt unchanged.
        settlement_tx.send_replace(Some(tip_through(1)));
        let refeed = next_bundle(&settlement_queue, Duration::from_secs(10))
            .expect("the deferred startup re-feed");
        refeed.wait_artifact_published_blocking();
        assert_eq!(
            refeed.artifact().expect("the re-fed bundle carries its stored artifact").prev_state,
            [0xAA; 32],
            "the startup pass re-feeds the entry rather than deciding it against the baseline",
        );

        // The baseline restamps (an identical settlement value; only the tip's daa moves) below
        // the follow depth stay deferred: our settlement may merely be unpublished.
        tip_daa.store(5, Ordering::Relaxed);
        settlement_tx.send_replace(Some(tip_through(1)));
        assert!(
            next_bundle(&settlement_queue, Duration::from_millis(500)).is_none(),
            "a restamp below the follow depth decides nothing",
        );

        // The tip reaches the follow depth past the baseline: nothing of ours surfaces (the
        // lane settles alone), so the restamp is decisive, the entry is dropped, and the
        // committed-gap retry re-covers the range as one fresh bundle chaining from the tip. A
        // republication re-feed of the fresh entry may follow the compose.
        tip_daa.store(10, Ordering::Relaxed);
        settlement_tx.send_replace(Some(tip_through(1)));
        let mut prev_states = Vec::new();
        loop {
            let timeout = if prev_states.is_empty() {
                Duration::from_secs(10)
            } else {
                Duration::from_millis(500)
            };
            let Some(bundle) = next_bundle(&settlement_queue, timeout) else { break };
            bundle.wait_artifact_published_blocking();
            let artifact =
                bundle.artifact().expect("the re-covered bundle carries a real artifact");
            assert_eq!(
                bundle.block_prove_to(),
                block_hash(3),
                "the re-covered bundle spans the dead entry's whole range",
            );
            prev_states.push(artifact.prev_state);
            assert!(prev_states.len() <= 2, "at most the compose and one republication re-feed");
        }
        assert_eq!(
            prev_states.first(),
            Some(&[0x00; 32]),
            "the range is re-composed from the tip, not re-fed from the dead receipt",
        );

        let recorded: Vec<(u64, u64)> =
            journal.entries().into_iter().map(|(s, e)| (s, e.end_index)).collect();
        assert_eq!(recorded, vec![(2, 3)], "the re-covered entry replaces the dead one");

        prover.shutdown();
        state.storage().shutdown();
    }
}

/// Seeds the dead shape over a scheduler that stays live across the restart point, so a test can
/// feed new work after the restart.
///
/// The entry's aggregate receipt is not stored; a test that needs it calls
/// [`store_stale_receipt`].
fn live_dead_shape(
    temp_dir: &TempDir,
) -> (Scheduler<RocksDbStore, PlainProcessor>, StoreJournal<RocksDbStore>) {
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
    let journal = StoreJournal::new(storage.clone());
    let mut scheduler = Scheduler::new(
        ExecutionConfig::default().with_processor(PlainProcessor),
        StorageConfig::default().with_store(storage),
    );
    commit_dead_shape(&mut scheduler, &journal);
    (scheduler, journal)
}

/// A prover restarted over a live scheduler, with the handles a test drives it through.
struct Restarted {
    /// Scheduler kept live across the restart point, feeding the new work.
    scheduler: Scheduler<RocksDbStore, PlainProcessor>,
    /// Journal the prover resumed from.
    journal: StoreJournal<RocksDbStore>,
    /// The restarted prover.
    prover: AggregateProver<RocksDbStore, PlainProcessor>,
    /// Settlement watch the test publishes tips on.
    settlement_tx: watch::Sender<Option<SettlementInfo>>,
    /// Queue the prover emits its bundles onto.
    settlement_queue: AsyncQueue<ScheduledBundle<SettlementArtifact<Vec<u8>>>>,
}

/// Boots a prover over the seeded journal, which is the restart, with the given tip observer and
/// a follow depth of 10, and publishes the bridge's baseline tip.
///
/// The baseline proves through block 1 and carries state `[0x00]`, which the journaled entry
/// (chaining from a base at `[0xAA]`) does not chain from.
fn restart(
    scheduler: Scheduler<RocksDbStore, PlainProcessor>,
    journal: StoreJournal<RocksDbStore>,
    tip_daa: Option<Arc<AtomicU64>>,
) -> Restarted {
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
            re_root: None,
            tip_daa,
            follow_depth: 10,
        },
    );
    settlement_tx.send_replace(Some(tip_through(1)));
    Restarted { scheduler, journal, prover, settlement_tx, settlement_queue }
}

/// Pops the startup pass's deferred re-feed: the stored receipt, unchanged.
fn expect_deferred_refeed(setup: &Restarted) {
    let refeed = next_bundle(&setup.settlement_queue, Duration::from_secs(10))
        .expect("the deferred startup re-feed");
    refeed.wait_artifact_published_blocking();
    assert_eq!(
        refeed.artifact().expect("the re-fed bundle carries its stored artifact").prev_state,
        [0xAA; 32],
        "the startup pass re-feeds the entry rather than deciding it against the baseline",
    );
}

/// Pops the committed-gap re-cover of the entry's range: one fresh bundle through block 3 that
/// chains from the tip.
fn expect_re_cover(setup: &Restarted) {
    let recovered = next_bundle(&setup.settlement_queue, Duration::from_secs(10))
        .expect("the entry's range must be re-covered");
    recovered.wait_artifact_published_blocking();
    assert_eq!(
        recovered.block_prove_to(),
        block_hash(3),
        "the re-covered bundle spans the entry's whole range",
    );
    assert_eq!(
        recovered.artifact().expect("the re-covered bundle carries a real artifact").prev_state,
        [0x00; 32],
        "the range is re-composed from the tip, not re-fed from the stored receipt",
    );
}

/// Feeds block 4 as new work once the startup passes are through.
fn feed_block_4(setup: &mut Restarted) {
    setup.prover.wait_booted();
    let batch = commit_batch_with_receipt(&mut setup.scheduler, block(4, 3));
    batch.publish_artifact(Some(batch_receipt(4)));
    setup.prover.submit(&batch);
}

/// Pops the bundle of the new work [`feed_block_4`] fed, failing with `missing` when none
/// arrives.
fn expect_block_4_bundle(setup: &Restarted, missing: &str) {
    let bundle = next_bundle(&setup.settlement_queue, Duration::from_secs(10)).expect(missing);
    bundle.wait_artifact_published_blocking();
    assert_eq!(bundle.block_prove_to(), block_hash(4), "the new work's bundle covers block 4");
    assert!(bundle.artifact().is_some(), "the new work's bundle carries a real artifact");
}

/// Checks that no further bundle arrives and that the journal holds exactly `spans`.
fn expect_settled(setup: &Restarted, spans: &[(u64, u64)], journal_shape: &str) {
    assert!(
        next_bundle(&setup.settlement_queue, Duration::from_millis(500)).is_none(),
        "no further handle may arrive",
    );
    let recorded: Vec<(u64, u64)> =
        setup.journal.entries().into_iter().map(|(s, e)| (s, e.end_index)).collect();
    assert_eq!(recorded, spans, "{journal_shape}");
}

/// Builds the state the hold tests decide from: a prover with a tip observer restarted over the
/// dead shape, whose startup pass deferred the decision and re-fed the entry, and block 4 fed as
/// new work whose bundle is held.
///
/// Returns the tip observer alongside, still at the baseline's daa 0.
fn held_new_work(temp_dir: &TempDir) -> (Restarted, Arc<AtomicU64>) {
    let (scheduler, journal) = live_dead_shape(temp_dir);
    store_stale_receipt(scheduler.state());
    let tip_daa = Arc::new(AtomicU64::new(0));
    let mut setup = restart(scheduler, journal, Some(tip_daa.clone()));
    expect_deferred_refeed(&setup);

    // Block 4 arrives with the decision still deferred: its bundle must wait rather than be
    // journaled above the entry.
    feed_block_4(&mut setup);
    assert!(
        next_bundle(&setup.settlement_queue, Duration::from_millis(500)).is_none(),
        "new work must wait while the dead-entry decision is deferred",
    );
    (setup, tip_daa)
}

/// Tests that new work arriving while the dead-entry decision is deferred waits for it.
///
/// A bundle journaled above the undecided entry moves the journal tail past the range the later
/// drop hands to the committed-gap retry, which then finds nothing above the tail and ends: the
/// dropped range stays uncovered, every later bundle chains above it, and the settler skips
/// them all. Held instead, the new work bundles only after the restamp decided, the entry
/// dropped, and the retry re-covered its range from the tip, so it settles behind the re-cover.
#[test]
fn new_work_waits_for_a_deferred_dead_entry_decision() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let (setup, tip_daa) = held_new_work(&temp_dir);

        // The tip reaches the follow depth past the baseline: the restamp decides, the entry
        // is dropped, and the retry re-covers its range from the tip ahead of the held work.
        tip_daa.store(10, Ordering::Relaxed);
        setup.settlement_tx.send_replace(Some(tip_through(1)));
        expect_re_cover(&setup);
        expect_block_4_bundle(&setup, "the held new work settles behind the re-cover");
        expect_settled(
            &setup,
            &[(2, 3), (4, 4)],
            "the re-covered entry replaces the dead one, with the new work journaled above it",
        );

        setup.prover.shutdown();
        setup.scheduler.shutdown();
    }
}

/// Tests that the hold on new work lifts when the deferred decision keeps the entry.
///
/// The common restart shape: the entry chains from our own settlement the baseline has not
/// published yet, so the decision is deferred and new work arriving meanwhile is held. Once
/// the bridge publishes that settlement, the decisive pass re-feeds the live entry and the held
/// work bundles behind it. A hold that lifted only on a drop would stall all new proving here.
#[test]
fn held_new_work_resumes_once_the_deferred_entry_proves_live() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let (setup, _tip_daa) = held_new_work(&temp_dir);

        // The bridge catches up and publishes the settlement the entry chains from: ours,
        // proving through block 2 under a new transaction id with settled state [0xAA]. The
        // advance pass is decisive, the entry re-feeds unchanged, and the hold lifts. The tip
        // observer never moves, so only this advance can decide.
        setup.settlement_tx.send_replace(Some(tip_with(2, [0xAA; 32])));
        let live = next_bundle(&setup.settlement_queue, Duration::from_secs(10))
            .expect("the live tail re-feeds after the advance");
        live.wait_artifact_published_blocking();
        assert_eq!(live.block_prove_to(), block_hash(3), "the journaled entry re-feeds");
        assert_eq!(
            live.artifact().expect("the re-fed bundle carries its stored artifact").prev_state,
            [0xAA; 32],
            "the live entry is re-fed, not re-composed",
        );
        expect_block_4_bundle(&setup, "the held new work bundles once the decision kept the entry");
        expect_settled(
            &setup,
            &[(3, 3), (4, 4)],
            "the live entry survives, with the new work journaled above it",
        );

        setup.prover.shutdown();
        setup.scheduler.shutdown();
    }
}

/// Tests that the hold on new work lifts when the watch stops carrying a settlement.
///
/// A reorg that rolls the bridge past its last settlement publishes none, which leaves no tip
/// to decide the deferred entry against. The hold must not outlive the baseline it waits on: on
/// a lane only we settle the held work is the only source of a new settlement, so holding it
/// would stall the prover until the next restart.
#[test]
fn held_new_work_resumes_once_the_watch_carries_no_settlement() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let (setup, _tip_daa) = held_new_work(&temp_dir);

        // The bridge rolls back past the baseline settlement and publishes none: the entry
        // stays undecided in the journal and the hold lifts.
        setup.settlement_tx.send_replace(None);
        expect_block_4_bundle(&setup, "the held new work bundles once no settlement is left");
        expect_settled(
            &setup,
            &[(3, 3), (4, 4)],
            "the undecided entry stays journaled, with the new work above it",
        );

        setup.prover.shutdown();
        setup.scheduler.shutdown();
    }
}

/// Tests that new work is not held during the deferral on a run without a tip observer.
///
/// Without the depth clock no restamp ever decides, so on a lane only we settle a hold would
/// never lift. Such a run bundles new work during the deferral, as it did before the hold
/// existed.
#[test]
fn new_work_is_not_held_without_a_tip_observer() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let (scheduler, journal) = live_dead_shape(&temp_dir);
        store_stale_receipt(scheduler.state());
        let mut setup = restart(scheduler, journal, None);
        expect_deferred_refeed(&setup);

        // Block 4 arrives with the decision still deferred and nothing published since the
        // baseline: its bundle forms right away.
        feed_block_4(&mut setup);
        expect_block_4_bundle(&setup, "new work bundles during the deferral without an observer");
        expect_settled(
            &setup,
            &[(3, 3), (4, 4)],
            "the undecided entry stays journaled, with the new work above it",
        );

        setup.prover.shutdown();
        setup.scheduler.shutdown();
    }
}

/// Tests that a startup re-feed which drops every entry leaves nothing deferred.
///
/// The entry's receipt is missing from the store, so the startup pass cannot tell whether it
/// chains from the baseline and takes the deferring path, where the re-feed drops it as
/// unreloadable. Nothing is left to decide: the committed-gap pass re-covers the range from the
/// tip, and new work bundles behind it without waiting for a decisive pass.
#[test]
fn new_work_is_not_held_once_the_deferred_re_feed_dropped_every_entry() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        // No receipt is stored for the journaled entry, and the tip observer is wired, so a
        // deferral would hold new work.
        let (scheduler, journal) = live_dead_shape(&temp_dir);
        let mut setup = restart(scheduler, journal, Some(Arc::new(AtomicU64::new(0))));
        expect_re_cover(&setup);

        // Block 4 arrives with nothing published since the baseline: its bundle forms right
        // away, behind the re-cover.
        feed_block_4(&mut setup);
        expect_block_4_bundle(&setup, "new work bundles with nothing left to decide");
        expect_settled(
            &setup,
            &[(2, 3), (4, 4)],
            "the re-covered entry replaces the dropped one, with the new work journaled above it",
        );

        setup.prover.shutdown();
        setup.scheduler.shutdown();
    }
}

/// Tests the live side of the deferred decision: an entry chaining from our own settlement the
/// bridge baseline has not published yet (the settler advanced on a probe confirm, the baseline
/// trails by the follow depth, and the crash landed inside that window) must survive the
/// startup pass and the advance that publishes the settlement: re-fed unchanged both times,
/// never dropped and never re-composed from the stale base. The pre-fix behavior read it as
/// dead at startup, dropped every entry, and paid one full aggregator re-proof of the range.
#[test]
fn a_live_entry_chaining_from_an_unpublished_settlement_survives_the_baseline_pass() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let (journal, state) = seeded_dead_shape(&temp_dir);

        let settlement_queue: AsyncQueue<ScheduledBundle<SettlementArtifact<Vec<u8>>>> =
            AsyncQueue::new();
        let (settlement_tx, settlement_rx) = watch::channel::<Option<SettlementInfo>>(None);
        let prover = AggregateProver::new(
            SyntheticBackend,
            state.receipt_store(),
            Some(journal.clone()),
            AggregateProverConfig {
                lane_key: Hash::default(),
                covenant_id: None,
                lane_source: ServeLaneProofs,
                settlement_queue: Some(settlement_queue.clone()),
                settlement: Some(settlement_rx),
                bundle_size: 1..=1,
                exits: None,
                re_root: None,
                tip_daa: None,
                follow_depth: 0,
            },
        );

        // The baseline tip proves through block 1 and carries state [0x00], which the entry
        // (chaining from our probe-confirmed settlement at [0xAA]) does not chain from: the
        // deferred startup pass re-feeds it unchanged.
        settlement_tx.send_replace(Some(tip_through(1)));
        let startup = next_bundle(&settlement_queue, Duration::from_secs(10))
            .expect("the deferred startup re-feed");
        startup.wait_artifact_published_blocking();
        assert_eq!(
            startup.artifact().expect("the re-fed bundle carries its stored artifact").prev_state,
            [0xAA; 32],
        );

        // The bridge catches up and publishes our settlement (a new transaction id, settled
        // state [0xAA]): the advance pass is decisive, the entry chains from the tip it carries,
        // and the tail re-feeds unchanged again.
        settlement_tx.send_replace(Some(tip_with(1, [0xAA; 32])));
        let advance = next_bundle(&settlement_queue, Duration::from_secs(10))
            .expect("the live tail re-feeds after the advance");
        advance.wait_artifact_published_blocking();
        assert_eq!(
            advance.artifact().expect("the re-fed bundle carries its stored artifact").prev_state,
            [0xAA; 32],
        );
        assert!(
            next_bundle(&settlement_queue, Duration::from_millis(500)).is_none(),
            "no fresh compose ever replaces the live entry",
        );

        let recorded: Vec<(u64, u64)> =
            journal.entries().into_iter().map(|(s, e)| (s, e.end_index)).collect();
        assert_eq!(recorded, vec![(3, 3)], "the live entry survives untouched");

        prover.shutdown();
        state.storage().shutdown();
    }
}

/// A lane source serving every fetch except one dead block: the block exists on chain but its
/// lane proof is unobtainable (pruned lane history), the shape the live store served for the
/// era's prove-through block.
struct DeadBlockLaneProofs {
    /// Block whose lane proof always fails.
    dead: Hash,
}

impl LaneProofSource for DeadBlockLaneProofs {
    async fn fetch_lane_proof(
        &self,
        req: LaneProofRequest,
    ) -> Result<GetSeqCommitLaneProofResponse, LaneProofError> {
        if req.block == self.dead {
            Err(LaneProofError("dead block: lane history pruned".into()))
        } else {
            Ok(GetSeqCommitLaneProofResponse {
                smt_proof: Vec::new(),
                lane: None,
                payload_and_ctx_digest: Hash::default(),
                parent_seq_commit: Hash::default(),
                inactivity_shortcut: Hash::default(),
            })
        }
    }
}

/// Tests that a committed-gap range whose final block's lane proof is unobtainable still makes
/// progress: the bundle's end walks down to the previous batch and covers the live prefix,
/// instead of deferring the same dead-ended range on every wake. The still-dead suffix stays
/// for a later pass, matching how live bundling parks on a dead final block.
#[test]
fn committed_gap_walks_end_down_over_a_dead_final_block() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        // The pre-restart half: block 1 settled and journaled, blocks 2 and 3 proved above it
        // with no journal record (the kill preceded it). Block 3's lane proof is dead.
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let journal = StoreJournal::new(storage.clone());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(PlainProcessor),
            StorageConfig::default().with_store(storage.clone()),
        );
        commit_batch_with_receipt(&mut scheduler, block(1, 0));
        commit_tip_chained_batch(&mut scheduler, block(2, 1));
        commit_batch_with_receipt(&mut scheduler, block(3, 2));
        journal.record(
            1,
            &JournalEntry {
                end_index: 1,
                from_block: block_hash(1),
                block_prove_to: block_hash(1),
                seq_commit: seq_commit(),
            },
        );
        scheduler.shutdown();

        let state = SchedulerState::<RocksDbStore, PlainProcessor>::new(
            StorageConfig::default().with_store(storage.clone()),
        );
        let settlement_queue: AsyncQueue<ScheduledBundle<SettlementArtifact<Vec<u8>>>> =
            AsyncQueue::new();
        let (settlement_tx, settlement_rx) = watch::channel::<Option<SettlementInfo>>(None);
        let prover = AggregateProver::new(
            SyntheticBackend,
            state.receipt_store(),
            Some(journal.clone()),
            AggregateProverConfig {
                lane_key: Hash::default(),
                covenant_id: None,
                lane_source: DeadBlockLaneProofs { dead: block_hash(3) },
                settlement_queue: Some(settlement_queue.clone()),
                settlement: Some(settlement_rx),
                bundle_size: 1..=1,
                exits: None,
                re_root: None,
                tip_daa: None,
                follow_depth: 0,
            },
        );

        settlement_tx.send_replace(Some(tip_through(1)));

        // The pass walks the end down from the dead block 3 to the live block 2 and covers the
        // prefix, recording its entry; no bundle ever proves through the dead block. A
        // republication re-feed of the covered entry may follow the compose.
        let mut proved_to = Vec::new();
        loop {
            let timeout = if proved_to.is_empty() {
                Duration::from_secs(10)
            } else {
                Duration::from_millis(500)
            };
            let Some(bundle) = next_bundle(&settlement_queue, timeout) else { break };
            bundle.wait_artifact_published_blocking();
            assert!(bundle.artifact().is_some(), "the covered prefix carries a real artifact");
            assert_eq!(
                bundle.block_prove_to(),
                block_hash(2),
                "the end walks down over the dead final block to the previous batch",
            );
            proved_to.push(bundle.checkpoint_index());
            assert!(proved_to.len() <= 2, "at most the compose and one republication re-feed");
        }
        assert_eq!(proved_to.first(), Some(&2), "the walk-down covered the live prefix");

        let recorded: Vec<(u64, u64)> =
            journal.entries().into_iter().map(|(s, e)| (s, e.end_index)).collect();
        assert_eq!(
            recorded,
            vec![(2, 2)],
            "the covered prefix is journaled; the dead suffix stays for a later pass",
        );

        prover.shutdown();
        state.storage().shutdown();
    }
}
