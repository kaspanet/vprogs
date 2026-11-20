//! Pins what the batch worker proves for a batch whose transactions touch no resource.
//!
//! The worker hands the batch guest an SMT proof over the resources the batch touches, and the
//! guest commits the roots it computes from that proof. One lane transaction whose access list
//! is empty or does not decode, alone in its chain block, is a batch that touches none. A proof
//! without a leaf recomputes to the empty root whatever state the lane holds, so the guest
//! would commit the empty root on both sides. Once the lane holds a state, such a receipt
//! chains with no other batch and the worker's root compare kills its thread. The proof over
//! no key therefore has to bind the root as well.
//!
//! The backend runs the batch guest's verifier on the host over the inputs the worker built, so
//! every receipt carries what the guest commits for them.

// The backend traits return `impl Future + 'static`, which an `async fn` cannot satisfy: its
// future borrows `&self`.
#![allow(clippy::manual_async_fn)]

use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use kaspa_hashes::Hash;
use tempfile::TempDir;
use tokio::runtime::Builder;
use vprogs_core_hashing::Sha256;
use vprogs_core_smt::{EMPTY_HASH, Tree};
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId, SchedulerTransaction};
use vprogs_l1_types::ChainBlockMetadata;
use vprogs_scheduling_scheduler::{
    ExecutionConfig, Processor, ScheduledBatch, Scheduler, TransactionContext,
};
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_abi::{
    Error,
    batch_processor::{BatchPins, BatchTransition, Inputs as BatchInputs, Verifier},
    transaction_processor::{OutputCommitment, Transaction},
};
use vprogs_zk_batch_prover::{BatchProver, BatchProverConfig};
use zerocopy::FromBytes;

/// Transaction-guest image id; the processor publishes tx artifacts directly, so it keys nothing.
const TX_IMAGE_ID: [u8; 32] = [0u8; 32];

/// Batch-guest image id, keying the per-batch receipt cache.
const BATCH_IMAGE_ID: [u8; 32] = [1u8; 32];

/// Lane key the prover is configured with.
const LANE_KEY: [u8; 32] = [2u8; 32];

/// Covenant id the prover is configured with.
const COVENANT_ID: [u8; 32] = [3u8; 32];

/// Runs the batch guest's verifier on the host over `inputs` and returns the journal it
/// commits. The tx receipts are not verified.
fn guest_journal(inputs: &[u8]) -> Vec<u8> {
    let mut verifier = Verifier::new(inputs, |_image_id: &[u8; 32], _journal: &[u8]| {});
    let (new_lane_tip, new_lane_blue_score) = verifier.verify_batch();
    let mut journal = Vec::new();
    verifier.commit_batch_transition::<Sha256>(&mut journal, &new_lane_tip, new_lane_blue_score);
    journal
}

/// Backend whose batch prove hands out, as the receipt, the journal the batch guest commits
/// for the inputs the worker built, counting every call.
#[derive(Clone)]
struct GuestBackend {
    /// Proves run so far, shared with the worker's clone.
    proves: Arc<AtomicUsize>,
}

impl vprogs_zk_transaction_prover::Backend for GuestBackend {
    type Receipt = Vec<u8>;

    fn image_id(&self) -> &[u8; 32] {
        &TX_IMAGE_ID
    }

    fn prove_transaction(
        &self,
        _input_bytes: Vec<u8>,
    ) -> impl Future<Output = Result<Vec<u8>, String>> + Send + 'static {
        async { unreachable!("the processor publishes tx artifacts directly") }
    }
}

impl vprogs_zk_batch_prover::Backend for GuestBackend {
    fn prove_batch(
        &self,
        inputs: &[u8],
        _receipts: Vec<Vec<u8>>,
    ) -> impl Future<Output = Result<Vec<u8>, String>> + Send + 'static {
        self.proves.fetch_add(1, Ordering::SeqCst);
        let journal = guest_journal(inputs);
        async move { Ok(journal) }
    }

    fn journal_bytes(receipt: &Vec<u8>) -> Vec<u8> {
        receipt.clone()
    }

    fn batch_image_id(&self) -> &[u8; 32] {
        &BATCH_IMAGE_ID
    }
}

/// Processor that writes every resource a transaction touches and publishes, as the
/// transaction's artifact, the journal the transaction guest commits for a carrier it rejects
/// at decode.
///
/// Such a journal has no execution context, so the batch guest counts the transaction as lane
/// activity and reads no resource for it. That is the artifact of a lane transaction whose
/// payload does not decode, which the bridge schedules without resources. The batch that
/// writes a resource only gives the lane a state and is never proved, so its artifact is not
/// read.
#[derive(Clone)]
struct RejectingProcessor;

impl Processor<RocksDbStore> for RejectingProcessor {
    fn process_transaction(
        &self,
        ctx: &mut TransactionContext<RocksDbStore, Self>,
    ) -> Result<(), Self::Error> {
        for resource in ctx.resources_mut() {
            resource.set_data(vec![1]);
        }
        let merge_idx = ctx.scheduler_tx().merge_idx;
        ctx.scheduled_tx().publish_artifact(Some(rejected_tx_journal(merge_idx)));
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

/// Encodes the journal of a carrier rejected at decode: its version, its tx id, its
/// `merge_idx`, no execution context, and a decode error as the output.
fn rejected_tx_journal(merge_idx: u32) -> Vec<u8> {
    let mut journal = Vec::new();
    journal.extend_from_slice(&Transaction::V1.to_le_bytes());
    journal.extend_from_slice(&[0xE0; 32]);
    journal.extend_from_slice(&merge_idx.to_le_bytes());
    journal.push(0);
    journal.push(OutputCommitment::ERROR);
    Error::Decode("malformed".into()).encode(&mut journal);
    journal
}

/// Chain-block metadata whose only distinguishing fields are the block hash and its parent id.
fn block(hash: u8, parent_id: u64) -> ChainBlockMetadata {
    ChainBlockMetadata { hash: Hash::from_bytes([hash; 32]), parent_id, ..Default::default() }
}

/// A lane transaction that touches no resource.
fn resource_less_tx(merge_idx: u32) -> SchedulerTransaction<usize> {
    SchedulerTransaction::new(merge_idx, vec![], 0)
}

/// Polls `batch.artifact_published()` until it opens, failing with `msg` after ten seconds.
fn wait_published(batch: &ScheduledBatch<RocksDbStore, RejectingProcessor>, msg: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !batch.artifact_published() {
        assert!(Instant::now() < deadline, "{msg}");
        thread::sleep(Duration::from_millis(20));
    }
}

/// The `(prev_state, new_state)` a batch receipt commits.
fn settled(receipt: &[u8]) -> ([u8; 32], [u8; 32]) {
    let transition = BatchTransition::ref_from_bytes(receipt).expect("decode BatchTransition");
    (transition.prev_state, transition.new_state)
}

/// The receipt the worker cached for `batch` while a proof over no key carried no leaf: what
/// the guest commits for the batch's inputs with such a proof.
fn receipt_over_a_leafless_proof(
    batch: &ScheduledBatch<RocksDbStore, RejectingProcessor>,
) -> Vec<u8> {
    // Five empty sections: no key, no leaf, no sibling, no topology byte and no membership.
    let proof = [0u8; 20];
    let journals: Vec<_> = batch.tx_artifacts().map(|artifact| (*artifact).clone()).collect();
    guest_journal(&BatchInputs::encode(
        BatchPins {
            tx_image_id: &TX_IMAGE_ID,
            covenant_id: &COVENANT_ID,
            deposit_spk_hash: &[0u8; 32],
            lane_key: &Hash::from_bytes(LANE_KEY),
        },
        &proof,
        batch.checkpoint().metadata(),
        &journals,
    ))
}

/// Reads the receipt the cache holds at `batch`'s coordinate.
fn cached_receipt(batch: &ScheduledBatch<RocksDbStore, RejectingProcessor>) -> Option<Vec<u8>> {
    let runtime = Builder::new_current_thread().build().expect("runtime");
    runtime.block_on(batch.read_batch_receipt().resolve())
}

/// Commits an anchor batch writing `anchor_writes`, proves a batch of one resource-less
/// transaction on top of it and a second such batch behind it, and returns the root the anchor
/// left.
///
/// Both batches have to settle that root, unchanged, so their state roots chain, and the
/// second one getting a receipt at all shows that the worker survived the first one's root
/// compare. Their lane tips do not chain: the blocks carry no lane pins.
///
/// With `cached_leafless` the cache already holds the first batch's receipt as it was proved
/// while a proof over no key carried no leaf.
fn settle_on_top_of(anchor_writes: Vec<ResourceId>, cached_leafless: bool) -> [u8; 32] {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
    let mut scheduler = Scheduler::new(
        ExecutionConfig::default().with_processor(RejectingProcessor),
        StorageConfig::default().with_store(storage.clone()),
    );

    // The anchor gives the lane whatever state the batches above it enter at.
    let anchor_txs = match anchor_writes.is_empty() {
        true => vec![],
        false => vec![SchedulerTransaction::new(
            0,
            anchor_writes.into_iter().map(AccessMetadata::write).collect(),
            0,
        )],
    };
    let anchor = scheduler.schedule(block(1, 0), anchor_txs);
    anchor.wait_committed_blocking();
    let entered = storage.root(1);

    // The victim holds lane activity and touches nothing, so the store keeps the anchor's root.
    let victim = scheduler.schedule(block(2, 1), vec![resource_less_tx(0)]);
    victim.wait_committed_blocking();
    assert_eq!(storage.root(2), entered, "a batch touching no resource leaves the root as it is");
    if cached_leafless {
        let stale = receipt_over_a_leafless_proof(&victim);
        assert_eq!(settled(&stale), (EMPTY_HASH, EMPTY_HASH), "a leafless proof binds no root");
        victim.write_batch_receipt(stale).wait_blocking();
    }

    let proves = Arc::new(AtomicUsize::new(0));
    let prover = BatchProver::new(
        GuestBackend { proves: proves.clone() },
        storage.clone(),
        BatchProverConfig {
            lane_key: Hash::from_bytes(LANE_KEY),
            covenant_id: Hash::from_bytes(COVENANT_ID),
            deposit_spk_hash: [0u8; 32],
        },
    );
    prover.submit(&victim);
    wait_published(&victim, "the worker must publish the victim's artifact");
    assert_eq!(
        settled(&victim.artifact()),
        (entered, entered),
        "a batch touching no resource enters at the lane's state and leaves it unchanged",
    );
    assert_eq!(
        cached_receipt(&victim).as_ref(),
        Some(&*victim.artifact()),
        "the cache holds the receipt the worker published",
    );

    // A second such batch, with more than one transaction, is served behind it and enters at
    // the root the victim left: the worker survived the victim's root compare.
    let probe = scheduler.schedule(block(3, 2), vec![resource_less_tx(0), resource_less_tx(1)]);
    probe.wait_committed_blocking();
    prover.submit(&probe);
    wait_published(&probe, "the worker must survive the victim and keep proving");
    assert_eq!(
        settled(&probe.artifact()),
        (entered, entered),
        "the next batch enters at the root the victim left and leaves it unchanged",
    );
    assert_eq!(proves.load(Ordering::SeqCst), 2, "each of the two batches is proved once");

    prover.shutdown();
    scheduler.shutdown();
    entered
}

/// On top of a lane state of one resource, which sits as a leaf at the tree's root, the batch
/// settles that state on both sides. While a proof over no key carried no leaf it settled the
/// empty root, and the worker died at its root compare.
#[test]
fn a_batch_touching_no_resource_settles_the_state_it_entered_at() {
    let entered = settle_on_top_of(vec![ResourceId::for_test(1)], false);
    assert_ne!(entered, EMPTY_HASH, "the lane holds a state");
}

/// The same on top of a state of several resources, where the root is an internal node.
#[test]
fn a_batch_touching_no_resource_settles_a_state_of_several_resources() {
    let resources = (1..=5).map(ResourceId::for_test).collect();
    assert_ne!(settle_on_top_of(resources, false), EMPTY_HASH, "the lane holds a state");
}

/// On a lane without any state the batch settles the empty root, as it always did.
#[test]
fn a_batch_touching_no_resource_on_an_empty_state_settles_the_empty_root() {
    assert_eq!(settle_on_top_of(vec![], false), EMPTY_HASH, "the lane holds no state");
}

/// A store can still hold a receipt that was proved over a leafless proof. It enters at the
/// empty root, which is not where the batch enters, so the worker proves the batch afresh once
/// it is served again and replaces the receipt.
#[test]
fn a_cached_receipt_proved_over_a_leafless_proof_is_replaced() {
    let entered = settle_on_top_of(vec![ResourceId::for_test(1)], true);
    assert_ne!(entered, EMPTY_HASH, "the lane holds a state");
}
