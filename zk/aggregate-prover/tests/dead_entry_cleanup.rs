//! Pins the funding-race robustness of the committed-gap recovery: a settlement that dies
//! without landing leaves a journal entry chaining from a base the covenant never took, and the
//! resume pass must drop that dead entry so the committed-gap pass re-covers its range as one
//! fresh bundle chaining from the on-chain tip. A committed-gap range whose final block's lane
//! proof is unobtainable (pruned lane history) must walk its end down to the live prefix
//! instead of deferring the same dead-ended range on every wake.

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
use vprogs_l1_types::{ChainBlockMetadata, SettlementInfo};
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

/// A settlement tip proving through `block`, so the resume pass resolves it against the journal.
fn tip_through(block: u8) -> SettlementInfo {
    SettlementInfo { block_prove_to: block_hash(block), ..Default::default() }
}

/// Encodes the settlement journal of a bundle that chains from a base the covenant never took:
/// identical to [`settlement_journal`] except its proven `prev_state`, so the resume pass's
/// tip-pins check rejects it.
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

/// Tests that a journal entry left by a settlement that died without landing (its proven
/// transition chains from a base the covenant never took) is dropped by the resume pass rather
/// than re-fed forever, and the committed-gap pass re-covers the range as one fresh bundle
/// chaining from the on-chain tip. Re-feeding the dead entry instead wedges the settler: every
/// bundle re-folds from the stale base, the settler skips it as a base mismatch, and the entry
/// never resolves.
#[test]
fn committed_gap_with_dead_settlement_entry_recovers() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        // The pre-restart half: block 1 settled, blocks 2 and 3 proved above it, and one journal
        // entry recorded above the settled tip whose receipt chains from a base the covenant
        // never took (the shape a dead settlement leaves behind: the bundle below it never
        // landed, so it chains from that bundle's end state, not the on-chain tip).
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let journal = StoreJournal::new(storage.clone());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(PlainProcessor),
            StorageConfig::default().with_store(storage.clone()),
        );
        commit_batch_with_receipt(&mut scheduler, block(1, 0));
        commit_batch_with_receipt(&mut scheduler, block(2, 1));
        commit_batch_with_receipt(&mut scheduler, block(3, 2));
        journal.record(
            3,
            &JournalEntry {
                end_index: 3,
                from_block: block_hash(3),
                block_prove_to: block_hash(3),
                seq_commit: seq_commit(),
            },
        );
        scheduler.shutdown();

        // The restart half: the dead entry's aggregate receipt is stored where the re-feed
        // would reload it.
        let state = SchedulerState::<RocksDbStore, PlainProcessor>::new(
            StorageConfig::default().with_store(storage.clone()),
        );
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
            },
        );

        // The bridge's baseline tip proves through block 1 and carries the covenant's on-chain
        // state, which the dead entry's receipt does not chain from.
        settlement_tx.send_replace(Some(tip_through(1)));

        // The dead entry is dropped and the range re-covered as one fresh bundle chaining from
        // the tip: the emitted artifact carries the fresh compose's prev_state, not the stored
        // dead receipt's. A republication re-feed of the fresh entry may follow it.
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
        commit_batch_with_receipt(&mut scheduler, block(2, 1));
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
