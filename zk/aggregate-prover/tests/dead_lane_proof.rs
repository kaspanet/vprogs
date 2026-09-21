//! Reproduces the aggregate-prover death when a bundle's final block is reorged away
//! mid-aggregation: the lane-proof fetch fails for the dead block and the worker thread
//! panics (the production `RemoteLaneSource` panics after exhausting its retries), killing
//! bundling and settlement for the rest of the process's life.

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
use vprogs_zk_batch_prover::{LaneProofRequest, LaneProofSource};

/// Transaction-guest image id. This repro proves nothing real, so image ids only key receipt
/// lookups.
const TX_IMAGE_ID: [u8; 32] = [0u8; 32];
/// Batch-guest image id, keying a per-batch receipt in the proof-receipt store.
const BATCH_IMAGE_ID: [u8; 32] = [1u8; 32];
/// Aggregator-guest image id, keying a bundle's settlement receipt.
const AGGREGATOR_IMAGE_ID: [u8; 32] = [2u8; 32];

/// Block whose lane proof is dead: a reorg orphaned it while its bundle was in flight.
const DEAD_BLOCK: u64 = 3;

/// Block the chain settles on after the reorg: the dead block's surviving replacement.
const REPLACEMENT_BLOCK: u64 = 4;

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
/// artifact instead of resolving the bundle as a no-op. Fields are encoded in declared order by
/// the journal's own encoder.
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

/// Lane source failing exactly the way the production one dies on a dead block, and serving a
/// canned response for every live block so earlier bundles settle normally.
struct DeadBlockLaneSource {
    /// Hash of the block a reorg orphaned while its bundle was in flight.
    dead: Hash,
}

impl LaneProofSource for DeadBlockLaneSource {
    async fn fetch_lane_proof(&self, req: LaneProofRequest) -> GetSeqCommitLaneProofResponse {
        // RED STAGE: the trait is infallible, so the production failure shape for the dead
        // block is the panic the RemoteLaneSource emits after exhausting retries. Live blocks
        // must keep serving, or the red failure fires for the wrong reason (block 1 never
        // settles). GREEN STAGE (Task 4): this arm returns Err and the test asserts deferral.
        if req.block == self.dead {
            panic!("get_seq_commit_lane_proof failed after 10 attempts: block not found");
        }
        GetSeqCommitLaneProofResponse {
            smt_proof: Vec::new(),
            lane: None,
            payload_and_ctx_digest: Hash::default(),
            parent_seq_commit: Hash::default(),
            inactivity_shortcut: Hash::default(),
        }
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

/// Commits a one-transaction batch for `meta`, stands in for the batch prover by publishing its
/// receipt, and feeds it to the aggregate prover.
fn commit_and_submit<S, P>(
    scheduler: &mut Scheduler<S, P>,
    prover: &AggregateProver<S, P>,
    meta: ChainBlockMetadata,
) where
    S: vprogs_storage_types::Store,
    P: vprogs_scheduling_scheduler::Processor<
            S,
            Transaction = usize,
            BatchArtifact = Vec<u8>,
            BatchMetadata = ChainBlockMetadata,
        >,
{
    let batch = scheduler.schedule(meta, vec![lane_tx()]);
    batch.wait_committed_blocking();
    batch.publish_artifact(Some(settlement_journal()));
    prover.submit(&batch);
}

/// Tests that a bundle whose final block was reorged away mid-aggregation (its lane-proof fetch
/// fails) does not kill the worker: the next block's bundle must still settle.
///
/// Today the lane-proof fetch is infallible, so the dead block's failure surfaces as the worker
/// thread panicking inside the fetch, and bundling and settlement stop for the rest of the
/// process's life.
#[test]
fn dead_lane_proof_fetch_does_not_kill_the_worker() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(PlainProcessor),
            StorageConfig::default().with_store(storage),
        );

        let settlement_queue: AsyncQueue<ScheduledBundle<SettlementArtifact<Vec<u8>>>> =
            AsyncQueue::new();
        let prover = AggregateProver::new(
            SyntheticBackend,
            scheduler.state().receipt_store(),
            AggregateProverConfig {
                lane_key: Hash::default(),
                covenant_id: None,
                lane_source: DeadBlockLaneSource { dead: block_hash(DEAD_BLOCK as u8) },
                settlement_queue: Some(settlement_queue.clone()),
                settlement: None,
                journal: None,
                bundle_size: 1..=1,
                exits: None,
            },
        );

        // Block 1: commits and settles normally; the lane source serves every live block, so a
        // failure here would be the wrong reason for the test to fail.
        commit_and_submit(&mut scheduler, &prover, block(1, 0));
        let bundle = next_bundle(&settlement_queue, Duration::from_secs(10))
            .expect("the live block's bundle must settle");
        bundle.wait_artifact_published_blocking();
        assert_eq!(bundle.block_prove_to(), block_hash(1));
        assert!(bundle.artifact().is_some(), "block 1's bundle carries a real artifact");

        // The dead block: a reorg orphaned it while its bundle was in flight, so its lane-proof
        // fetch fails. Its bundle forms (the receipt is published), and the fetch kills the
        // worker today.
        commit_and_submit(&mut scheduler, &prover, block(DEAD_BLOCK as u8, 1));

        // The worker must survive and settle the next block's bundle.
        commit_and_submit(&mut scheduler, &prover, block(REPLACEMENT_BLOCK as u8, 1));
        let bundle = next_bundle(&settlement_queue, Duration::from_secs(10))
            .expect("the worker must survive the dead lane-proof fetch and keep settling");
        assert_eq!(
            bundle.block_prove_to(),
            block_hash(REPLACEMENT_BLOCK as u8),
            "the dead block's bundle killed the worker: its handle was emitted but never \
             resolved, and the replacement block never settled",
        );
        bundle.wait_artifact_published_blocking();
        assert!(bundle.artifact().is_some(), "the replacement's bundle carries a real artifact");

        prover.shutdown();
        scheduler.shutdown();
    }
}
