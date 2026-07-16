//! Reproduces the aggregate prover discarding a bundle that is provably not a no-op.
//!
//! The wrapper classifies a bundle as a no-op on state-root equality alone, on the premise that an
//! unchanged state root means the bundle's blocks carried no lane activity. That premise is false.
//! The batch guest folds *every* transaction into the lane activity digest, including a failed one,
//! and any batch with at least one tx journal derives a new lane tip from that digest. A failed
//! transaction writes no resource, so the SMT root is unchanged while the lane tip still advances.
//!
//! Such a bundle is consumed and reported as a no-op, so the covenant is never advanced. The next
//! bundle chains its `prev_lane_tip` to the advanced tip while the covenant's `lane_tip` stays
//! stale, and the settler's chain check then rejects every later settlement: liveness is lost
//! permanently, not just for the dropped bundle.

// The backend traits return `impl Future + 'static`, which an `async fn` cannot satisfy: its future
// borrows `&self`.
#![allow(clippy::manual_async_fn)]

use std::{
    future::Future,
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant},
};

use kaspa_hashes::Hash;
use kaspa_rpc_core::{GetSeqCommitLaneProofResponse, RpcLaneEntry};
use tempfile::TempDir;
use vprogs_core_atomics::AsyncQueue;
use vprogs_core_codec::Reader;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId, SchedulerTransaction};
use vprogs_l1_types::ChainBlockMetadata;
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler, TransactionContext};
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_abi::{
    batch_aggregator::{StateTransition, Verifier},
    batch_processor::{BatchTransition, BatchTransitionArgs},
    withdrawal::{ExitAccumulator, StandardSpk},
};
use vprogs_zk_aggregate_prover::{
    AggregateProver, AggregateProverConfig, ScheduledBundle, SettlementArtifact,
};
use vprogs_zk_batch_prover::{LaneProofRequest, LaneProofSource};

/// Transaction-guest image id. This repro proves nothing, so image ids only key receipt lookups.
const TX_IMAGE_ID: [u8; 32] = [0u8; 32];
/// Batch-guest image id, keying a per-batch receipt in the proof-receipt store.
const BATCH_IMAGE_ID: [u8; 32] = [1u8; 32];
/// Aggregator-guest image id, keying a bundle's settlement receipt.
const AGGREGATOR_IMAGE_ID: [u8; 32] = [2u8; 32];

/// Covenant the bundle's journal binds to.
const COVENANT_ID: [u8; 32] = [0xCC; 32];

/// Counts the wrapper's calls into the aggregator backend. A bundle filtered out by the earlier
/// tx-count guard is consumed without proving, so a non-zero count pins that the bundle reached the
/// journal parse and was dropped by the state-root branch rather than by that guard.
static AGGREGATOR_PROVE_CALLS: AtomicUsize = AtomicUsize::new(0);

/// The lane's SMT state root. The batch's only transaction failed, so it writes no resource and
/// this root is both the bundle's `prev_state` and its `new_state`.
const STATE: [u8; 32] = [0x11; 32];

/// Lane key this bundle settles.
fn lane_key() -> Hash {
    Hash::from_bytes([0xBB; 32])
}

/// Lane tip entering the bundle.
fn prev_lane_tip() -> Hash {
    Hash::from_bytes([0x40; 32])
}

/// Lane tip the batch's failed transaction advances the lane to: folding the transaction into the
/// activity digest derives a fresh tip even though it changed no resource.
fn new_lane_tip() -> Hash {
    Hash::from_bytes([0x50; 32])
}

/// Records exits in arrival order. The batch emits none, so `finalize` returns the "no exits"
/// sentinel the settlement journal carries as `permission_spk_hash`.
struct RecordedExits {
    entries: Vec<(Vec<u8>, u64)>,
}

impl ExitAccumulator for RecordedExits {
    fn add_exit(&mut self, dest: StandardSpk<'_>, amount: u64) {
        let mut buf = Vec::new();
        dest.encode(&mut buf);
        self.entries.push((buf, amount));
    }

    fn finalize(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        out[0] = self.entries.len() as u8;
        out
    }
}

/// No-op `verify_batch_journal` closure: skips the `env::verify` step so the real aggregator
/// verifier runs host-side.
fn skip_verify(_image_id: &[u8; 32], _journal: &[u8]) {}

/// Encodes the [`BatchTransition`] journal the batch guest commits for a batch whose only
/// transaction failed: the state root is carried through unchanged while the lane tip advances.
fn failed_tx_batch_journal() -> Vec<u8> {
    let mut buf = Vec::new();
    BatchTransition::encode(
        &mut buf,
        BatchTransitionArgs {
            prev_state: &STATE,
            prev_lane_tip: &prev_lane_tip(),
            prev_lane_blue_score: 100,
            // A failed tx commits no resource change, so the batch's new root equals its previous
            // one, while the tx still folds into the activity digest and moves the tip.
            new_state: &STATE,
            new_lane_tip: &new_lane_tip(),
            new_lane_blue_score: 200,
            lane_key: &lane_key(),
            covenant_id: &COVENANT_ID,
            tx_image_id: &TX_IMAGE_ID,
            deposit_spk_hash: &[0u8; 32],
            lane_expired: false,
            exits: &[],
        },
    );
    buf
}

/// Encodes the aggregator's inputs over `journals`. The three lane-proof hashes feed only the
/// bundle's `new_seq_commit` derivation, so they are fixed rather than taken from a live node; the
/// smt_proof is parsed at decode, so it is a structurally valid empty proof (an all-set bitmap with
/// every sibling elided, plus the `Full` terminal tag).
fn aggregator_inputs(journals: &[&[u8]]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&BATCH_IMAGE_ID);
    buf.extend_from_slice(&[0u8; 32]); // payload_and_ctx_digest
    buf.extend_from_slice(&[0u8; 32]); // parent_seq_commit
    buf.extend_from_slice(&[0u8; 32]); // inactivity_shortcut

    let mut smt_proof = [0xFFu8; 33];
    smt_proof[32] = 0; // Full terminal tag
    buf.extend_from_slice(&(smt_proof.len() as u32).to_le_bytes());
    buf.extend_from_slice(&smt_proof);

    for journal in journals {
        buf.extend_from_slice(&(journal.len() as u32).to_le_bytes());
        buf.extend_from_slice(journal);
    }
    buf
}

/// Runs the real aggregator verifier over the failed-transaction batch and returns the
/// [`StateTransition`] settlement journal it commits. This is the journal the aggregator guest
/// produces for this bundle, not a hand-written stand-in: only the proving step is skipped.
fn settlement_journal() -> Vec<u8> {
    let batch = failed_tx_batch_journal();
    let inputs = aggregator_inputs(&[&batch]);
    let mut verifier = Verifier::new(&inputs, skip_verify, RecordedExits { entries: Vec::new() });
    let last = verifier.verify_batches();
    let mut journal = Vec::new();
    verifier.commit_state_transition(&mut journal, last);
    journal
}

/// Decodes `journal` into its [`StateTransition`] fields.
fn decode(journal: &[u8]) -> &StateTransition {
    (&mut &journal[..]).array_as::<StateTransition>("state_transition").expect("decode journal")
}

/// Backend standing in for the aggregator guest: it returns the journal the real verifier commits
/// for this bundle, so the wrapper parses exactly what production would hand it. The receipt is its
/// own journal.
#[derive(Clone)]
struct JournalBackend;

impl vprogs_zk_transaction_prover::Backend for JournalBackend {
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

impl vprogs_zk_batch_prover::Backend for JournalBackend {
    fn prove_batch(
        &self,
        _inputs: &[u8],
        _receipts: Vec<Self::Receipt>,
    ) -> impl Future<Output = Self::Receipt> + Send + 'static {
        async { unreachable!("the repro publishes the batch receipt directly") }
    }

    fn journal_bytes(receipt: &Self::Receipt) -> Vec<u8> {
        receipt.clone()
    }

    fn batch_image_id(&self) -> &[u8; 32] {
        &BATCH_IMAGE_ID
    }
}

impl vprogs_zk_aggregate_prover::Backend for JournalBackend {
    fn prove_aggregator(
        &self,
        _inputs: &[u8],
        _batch_receipts: Vec<Self::Receipt>,
    ) -> impl Future<Output = Self::Receipt> + Send + 'static {
        AGGREGATOR_PROVE_CALLS.fetch_add(1, Ordering::SeqCst);
        async { settlement_journal() }
    }

    fn aggregator_image_id(&self) -> &[u8; 32] {
        &AGGREGATOR_IMAGE_ID
    }
}

/// Lane source returning the same fixed proof the bundle's journal was derived from.
struct FixedLaneProof;

impl LaneProofSource for FixedLaneProof {
    async fn fetch_lane_proof(&self, _req: LaneProofRequest) -> GetSeqCommitLaneProofResponse {
        let mut smt_proof = vec![0xFFu8; 33];
        smt_proof[32] = 0; // Full terminal tag
        GetSeqCommitLaneProofResponse {
            smt_proof,
            lane: Some(RpcLaneEntry { tip: new_lane_tip(), blue_score: 200 }),
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

/// Chain-block metadata for a block carrying `seq_commit`, which the wrapper checks the bundle's
/// journal against.
fn block(hash: u8, parent_id: u64, seq_commit: Hash) -> ChainBlockMetadata {
    ChainBlockMetadata {
        hash: Hash::from_bytes([hash; 32]),
        parent_id,
        seq_commit,
        lane_key: lane_key(),
        ..Default::default()
    }
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

/// Pins the premise the wrapper's no-op branch denies: the aggregator commits an unchanged state
/// root together with an advanced lane tip for a batch whose only transaction failed. State root
/// and lane tip move independently, so root equality alone cannot mean "no lane activity".
#[test]
fn aggregator_commits_unchanged_state_with_an_advanced_lane_tip() {
    let journal = settlement_journal();
    let st = decode(&journal);

    assert_eq!(st.prev_state, STATE, "the failed tx writes no resource");
    assert_eq!(st.new_state, st.prev_state, "an unchanged state root");
    assert_eq!(st.prev_lane_tip, prev_lane_tip());
    assert_eq!(st.new_lane_tip, new_lane_tip());
    assert_ne!(
        st.new_lane_tip, st.prev_lane_tip,
        "the lane tip advances: the bundle is not a no-op despite the unchanged state root",
    );
}

/// Tests that a bundle whose state root is unchanged but whose lane tip advanced is settled rather
/// than discarded as a no-op.
///
/// The bundle holds one batch whose only transaction failed. The wrapper reaches its no-op branch,
/// sees `new_state == prev_state`, and publishes `None`, dropping a bundle that advanced the lane
/// and must be settled to keep the covenant's `lane_tip` in step with the lane.
#[test]
#[ignore = "repro: the wrapper classifies an advanced lane tip as a no-op on state-root equality \
            alone and drops the bundle"]
fn bundle_advancing_only_the_lane_tip_is_settled() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(PlainProcessor),
            StorageConfig::default().with_store(storage),
        );

        // The block's seq_commit must match the one the bundle's journal derives, which the
        // wrapper asserts once it settles the bundle.
        let journal = settlement_journal();
        let seq_commit = decode(&journal).new_seq_commit;

        // One batch carrying one transaction: enough to pass the wrapper's earlier tx-count guard
        // and reach the no-op branch.
        let batch = scheduler.schedule(
            block(1, 0, seq_commit),
            vec![SchedulerTransaction::new(
                0,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                0,
            )],
        );
        batch.wait_committed_blocking();

        // Stand in for the batch prover: publish the batch's receipt so the wrapper composes it.
        batch.publish_artifact(Some(failed_tx_batch_journal()));

        let settlement_queue: AsyncQueue<ScheduledBundle<SettlementArtifact<Vec<u8>>>> =
            AsyncQueue::new();
        let prover = AggregateProver::new(
            JournalBackend,
            scheduler.state().receipt_store(),
            AggregateProverConfig {
                lane_key: lane_key(),
                covenant_id: Some(Hash::from_bytes(COVENANT_ID)),
                lane_source: FixedLaneProof,
                settlement_queue: Some(settlement_queue.clone()),
                settlement: None,
                bundle_size: 1..=usize::MAX,
            },
        );
        prover.submit(&batch);

        let bundle =
            next_bundle(&settlement_queue, Duration::from_secs(10)).expect("the batch must bundle");
        bundle.wait_artifact_published_blocking();

        // Discriminates the branch under test from the other paths that publish no artifact: the
        // bundle was proved, so the tx-count guard passed and the wrapper parsed this journal.
        assert_eq!(
            AGGREGATOR_PROVE_CALLS.load(Ordering::SeqCst),
            1,
            "the bundle must reach the aggregator: an unproved bundle would mean the tx-count \
             guard consumed it, not the state-root branch",
        );

        let artifact = bundle.artifact().expect(
            "a bundle whose lane tip advanced must be settled, not discarded as a no-op: its \
             covenant is never advanced and every later bundle's prev_lane_tip then chains from a \
             tip the covenant does not carry",
        );

        assert_eq!(artifact.prev_state, STATE);
        assert_eq!(artifact.new_state, artifact.prev_state, "the failed tx changed no state");
        assert_eq!(artifact.prev_lane_tip, prev_lane_tip());
        assert_eq!(artifact.new_lane_tip, new_lane_tip(), "the settlement advances the lane tip");

        prover.shutdown();
        scheduler.shutdown();
    }
}
