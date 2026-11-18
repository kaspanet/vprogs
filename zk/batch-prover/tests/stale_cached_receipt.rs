//! Pins what the batch worker does with a cached receipt that no longer proves its batch.
//!
//! The receipt cache is keyed by the batch's coordinate (checkpoint, block, batch image), which
//! does not name where the batch enters: the state root below it and the lane pins its block's
//! metadata carries. A re-root re-executes a block from the settlement boundary under the same
//! coordinate, so a receipt proved from another state or lane tip comes back as a cache hit.
//! Publishing it leaves the batch off the covenant tip for good, and a receipt that also exits
//! at another root trips the worker's root compare and kills its thread. The worker must reuse
//! a cached receipt only while it enters where the batch does, and prove afresh otherwise,
//! replacing the cached one.

// The backend traits return `impl Future + 'static`, which an `async fn` cannot satisfy: its
// future borrows `&self`.
#![allow(clippy::manual_async_fn)]

use std::{
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use kaspa_hashes::Hash;
use tempfile::TempDir;
use tokio::runtime::Builder;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId, SchedulerTransaction};
use vprogs_l1_types::ChainBlockMetadata;
use vprogs_scheduling_scheduler::{
    ExecutionConfig, Processor, ScheduledBatch, Scheduler, TransactionContext,
};
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::Store;
use vprogs_zk_abi::batch_processor::{BatchTransition, BatchTransitionArgs};
use vprogs_zk_batch_prover::{BatchProver, BatchProverConfig};

/// Transaction-guest image id; the processor publishes tx artifacts directly, so it keys nothing.
const TX_IMAGE_ID: [u8; 32] = [0u8; 32];

/// Batch-guest image id, keying the per-batch receipt cache.
const BATCH_IMAGE_ID: [u8; 32] = [1u8; 32];

/// Where a batch receipt enters: the fields its journal commits as the batch's starting point.
#[derive(Clone, Copy)]
struct Entry {
    /// State root below the batch.
    prev_state: [u8; 32],
    /// Lane tip entering the batch's block.
    prev_lane_tip: Hash,
    /// Blue score at which the lane was last active before the batch.
    prev_lane_blue_score: u64,
    /// Whether the lane re-anchored at the batch's block.
    lane_expired: bool,
}

/// A batch receipt entering at `entry` and settling `new_state`. The backend's `journal_bytes`
/// is the identity, so the receipt is its own journal.
fn receipt(entry: Entry, new_state: [u8; 32]) -> Vec<u8> {
    let mut journal = Vec::new();
    BatchTransition::encode(
        &mut journal,
        BatchTransitionArgs {
            prev_state: &entry.prev_state,
            prev_lane_tip: &entry.prev_lane_tip,
            prev_lane_blue_score: entry.prev_lane_blue_score,
            new_state: &new_state,
            new_lane_tip: &Hash::default(),
            new_lane_blue_score: 0,
            lane_key: &Hash::default(),
            covenant_id: &[0u8; 32],
            tx_image_id: &TX_IMAGE_ID,
            deposit_spk_hash: &[0u8; 32],
            lane_expired: entry.lane_expired,
            exits: &[],
        },
    );
    journal
}

/// Backend whose batch prove hands out the receipt the test scripted, counting every call.
#[derive(Clone)]
struct ScriptedBackend {
    /// Receipt the next prove returns.
    fresh: Arc<Mutex<Vec<u8>>>,
    /// Proves run so far, shared with the worker's clone.
    proves: Arc<AtomicUsize>,
}

impl vprogs_zk_transaction_prover::Backend for ScriptedBackend {
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

impl vprogs_zk_batch_prover::Backend for ScriptedBackend {
    fn prove_batch(
        &self,
        _inputs: &[u8],
        _receipts: Vec<Vec<u8>>,
    ) -> impl Future<Output = Result<Vec<u8>, String>> + Send + 'static {
        self.proves.fetch_add(1, Ordering::SeqCst);
        let fresh = self.fresh.lock().expect("fresh receipt mutex").clone();
        async move { Ok(fresh) }
    }

    fn journal_bytes(receipt: &Vec<u8>) -> Vec<u8> {
        receipt.clone()
    }

    fn batch_image_id(&self) -> &[u8; 32] {
        &BATCH_IMAGE_ID
    }
}

/// Processor that writes every resource a transaction touches, so each batch settles its own
/// state root, and publishes the transaction's artifact immediately.
#[derive(Clone)]
struct WritingProcessor;

impl Processor<RocksDbStore> for WritingProcessor {
    fn process_transaction(
        &self,
        ctx: &mut TransactionContext<RocksDbStore, Self>,
    ) -> Result<(), Self::Error> {
        for resource in ctx.resources_mut() {
            resource.set_data(vec![1]);
        }
        ctx.scheduled_tx().publish_artifact(Some(Vec::new()));
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

/// A [`block`] at which the lane re-anchors.
fn expired_block(hash: u8, parent_id: u64) -> ChainBlockMetadata {
    ChainBlockMetadata { lane_expired: true, ..block(hash, parent_id) }
}

/// The lane tip every test block enters at.
fn entering_lane_tip() -> Hash {
    Hash::from_bytes([0x11; 32])
}

/// The blue score the lane was last active at before every test block.
const ENTERING_LANE_BLUE_SCORE: u64 = 5;

/// Chain-block metadata for the given block hash and parent id. Its lane tips and its scores
/// each hold their own non-zero value, so a receipt checked against the wrong one does not
/// match. The lane does not re-anchor at it.
fn block(hash: u8, parent_id: u64) -> ChainBlockMetadata {
    ChainBlockMetadata {
        hash: Hash::from_bytes([hash; 32]),
        parent_id,
        blue_score: 13,
        daa_score: 17,
        prev_lane_tip: entering_lane_tip(),
        prev_lane_blue_score: ENTERING_LANE_BLUE_SCORE,
        lane_tip: Hash::from_bytes([0x22; 32]),
        lane_blue_score: 9,
        ..Default::default()
    }
}

/// One lane transaction, so its batch is non-empty and reaches the receipt lookup.
fn lane_tx(resource: usize) -> SchedulerTransaction<usize> {
    SchedulerTransaction::new(0, vec![AccessMetadata::write(ResourceId::for_test(resource))], 0)
}

/// Polls `batch.artifact_published()` until it opens, failing with `msg` after ten seconds.
fn wait_published(batch: &ScheduledBatch<RocksDbStore, WritingProcessor>, msg: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !batch.artifact_published() {
        assert!(Instant::now() < deadline, "{msg}");
        thread::sleep(Duration::from_millis(20));
    }
}

/// The state root `store` holds at `version`.
fn root_at<S: Store>(store: &S, version: u64) -> [u8; 32] {
    store.root(version)
}

/// Where a batch over `metadata` enters when the state below it roots at `prev_state`.
fn entry_of(metadata: &ChainBlockMetadata, prev_state: [u8; 32]) -> Entry {
    Entry {
        prev_state,
        prev_lane_tip: metadata.prev_lane_tip,
        prev_lane_blue_score: metadata.prev_lane_blue_score,
        lane_expired: metadata.lane_expired,
    }
}

/// Reads the receipt the cache holds at `batch`'s coordinate.
fn cached_receipt(batch: &ScheduledBatch<RocksDbStore, WritingProcessor>) -> Option<Vec<u8>> {
    let runtime = Builder::new_current_thread().build().expect("runtime");
    runtime.block_on(batch.read_batch_receipt().resolve())
}

/// A committed batch with a receipt already cached at its coordinate, the worker about to serve
/// it, and what a fresh proof of it would be.
struct Fixture {
    /// Keeps the store's directory alive.
    _temp_dir: TempDir,
    /// The store, for reading roots.
    storage: RocksDbStore,
    /// Scheduler the batches were committed through.
    scheduler: Scheduler<RocksDbStore, WritingProcessor>,
    /// The batch whose cached receipt the worker examines.
    victim: ScheduledBatch<RocksDbStore, WritingProcessor>,
    /// Where the victim really enters.
    entry: Entry,
    /// Root the store holds for the victim.
    settled_root: [u8; 32],
    /// Receipt a fresh proof of the victim yields: it enters at `entry` and settles the root
    /// the store holds for the victim.
    fresh: Vec<u8>,
    /// Proves the backend has run.
    proves: Arc<AtomicUsize>,
    /// The batch prover under test.
    prover: BatchProver<RocksDbStore, WritingProcessor>,
}

/// A [`fixture_over`] whose victim is an ordinary block: the lane does not re-anchor at it.
fn fixture(cached: impl FnOnce(Entry, [u8; 32]) -> Vec<u8>) -> Fixture {
    fixture_over(block(2, 1), cached)
}

/// Commits an anchor and a victim batch over `victim_block`, caches
/// `cached(entry, settled_root)` at the victim's coordinate, and starts a prover whose fresh
/// proofs enter where the victim does.
fn fixture_over(
    victim_block: ChainBlockMetadata,
    cached: impl FnOnce(Entry, [u8; 32]) -> Vec<u8>,
) -> Fixture {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
    let mut scheduler = Scheduler::new(
        ExecutionConfig::default().with_processor(WritingProcessor),
        StorageConfig::default().with_store(storage.clone()),
    );

    // A committed anchor below the victim, then the victim itself.
    let anchor = scheduler.schedule(block(1, 0), vec![lane_tx(1)]);
    anchor.wait_committed_blocking();
    let victim = scheduler.schedule(victim_block, vec![lane_tx(2)]);
    victim.wait_committed_blocking();

    // The victim enters at the anchor's root with its block's lane pins, and settles its own
    // root.
    let entry = entry_of(&victim_block, root_at(&storage, 1));
    let settled_root = root_at(&storage, 2);
    let fresh = receipt(entry, settled_root);
    victim.write_batch_receipt(cached(entry, settled_root)).wait_blocking();

    let proves = Arc::new(AtomicUsize::new(0));
    let prover = BatchProver::new(
        ScriptedBackend { fresh: Arc::new(Mutex::new(fresh.clone())), proves: proves.clone() },
        storage.clone(),
        BatchProverConfig {
            lane_key: Hash::default(),
            covenant_id: Hash::default(),
            deposit_spk_hash: [0u8; 32],
        },
    );
    Fixture {
        _temp_dir: temp_dir,
        storage,
        scheduler,
        victim,
        entry,
        settled_root,
        fresh,
        proves,
        prover,
    }
}

impl Fixture {
    /// Serves the victim and checks that the worker proved it afresh instead of publishing the
    /// cached receipt, replaced the cached receipt, and is still alive afterwards.
    fn assert_proved_afresh(mut self) {
        self.prover.submit(&self.victim);
        wait_published(&self.victim, "the worker must publish the victim's artifact");
        assert_eq!(
            *self.victim.artifact(),
            self.fresh,
            "a cached receipt entering elsewhere must not be published for the batch",
        );
        assert_eq!(self.proves.load(Ordering::SeqCst), 1, "the batch is proved exactly once");
        assert_eq!(
            cached_receipt(&self.victim),
            Some(self.fresh.clone()),
            "the fresh receipt replaces the stale one in the cache",
        );

        // A batch behind the victim still gets served: the worker survived the victim's root
        // compare. Its own receipt is already cached, so serving it takes no further proof.
        let probe_block = block(3, 2);
        let probe = self.scheduler.schedule(probe_block, vec![lane_tx(3)]);
        probe.wait_committed_blocking();
        let probe_entry = entry_of(&probe_block, self.settled_root);
        probe.write_batch_receipt(receipt(probe_entry, root_at(&self.storage, 3))).wait_blocking();
        self.prover.submit(&probe);
        wait_published(&probe, "the worker must survive the stale receipt and keep serving");
        assert_eq!(self.proves.load(Ordering::SeqCst), 1, "the probe is served from the cache");

        self.prover.shutdown();
        self.scheduler.shutdown();
    }

    /// Serves the victim and checks that the worker published the cached receipt as it is,
    /// without proving.
    fn assert_reused(self) {
        self.prover.submit(&self.victim);
        wait_published(&self.victim, "the worker must publish the victim's artifact");
        assert_eq!(
            *self.victim.artifact(),
            receipt(self.entry, self.settled_root),
            "the cached receipt is published as it is",
        );
        assert_eq!(self.proves.load(Ordering::SeqCst), 0, "a valid cache hit proves nothing");

        self.prover.shutdown();
        self.scheduler.shutdown();
    }
}

/// A cached receipt proved from another state root is not this batch's receipt, even when it
/// settles the root the store holds for the batch: the worker proves the batch afresh. What
/// decides is where the receipt enters, not where it exits.
#[test]
fn cached_receipt_from_another_state_is_proved_afresh() {
    fixture(|entry, settled_root| receipt(Entry { prev_state: [0xEE; 32], ..entry }, settled_root))
        .assert_proved_afresh();
}

/// A cached receipt of another lineage altogether enters and exits elsewhere. Published as it
/// was, its settled root would diverge from the store's and kill the worker at the root
/// compare: the worker proves the batch afresh and lives.
#[test]
fn cached_receipt_from_another_lineage_is_proved_afresh() {
    fixture(|entry, _| receipt(Entry { prev_state: [0xEE; 32], ..entry }, [0xDD; 32]))
        .assert_proved_afresh();
}

/// A cached receipt entering at another lane tip is not this batch's receipt, even when it
/// settles the right root: the worker proves the batch afresh.
#[test]
fn cached_receipt_from_another_lane_tip_is_proved_afresh() {
    fixture(|entry, settled_root| {
        receipt(Entry { prev_lane_tip: Hash::from_bytes([0x99; 32]), ..entry }, settled_root)
    })
    .assert_proved_afresh();
}

/// A cached receipt carrying another lane blue score at its entry is proved afresh.
#[test]
fn cached_receipt_from_another_lane_blue_score_is_proved_afresh() {
    fixture(|entry, settled_root| {
        receipt(Entry { prev_lane_blue_score: 7, ..entry }, settled_root)
    })
    .assert_proved_afresh();
}

/// A cached receipt proved with the lane re-anchored, for a block where it is not, is proved
/// afresh.
#[test]
fn cached_receipt_with_another_lane_expiry_is_proved_afresh() {
    fixture(|entry, settled_root| receipt(Entry { lane_expired: true, ..entry }, settled_root))
        .assert_proved_afresh();
}

/// The other direction: a cached receipt proved without the lane re-anchoring, for a block
/// where it does, is proved afresh.
#[test]
fn cached_receipt_without_the_lane_expiry_of_its_block_is_proved_afresh() {
    fixture_over(expired_block(2, 1), |entry, settled_root| {
        receipt(Entry { lane_expired: false, ..entry }, settled_root)
    })
    .assert_proved_afresh();
}

/// A block the lane re-anchors at still names the lane tip it enters at, and the committed-gap
/// pass compares it with the covenant tip like any other: a cached receipt entering at another
/// one is proved afresh there too.
#[test]
fn cached_receipt_from_another_lane_tip_is_proved_afresh_at_a_re_anchoring_block() {
    fixture_over(expired_block(2, 1), |entry, settled_root| {
        receipt(Entry { prev_lane_tip: Hash::from_bytes([0x99; 32]), ..entry }, settled_root)
    })
    .assert_proved_afresh();
}

/// The same for the lane blue score a re-anchoring block enters at.
#[test]
fn cached_receipt_from_another_lane_blue_score_is_proved_afresh_at_a_re_anchoring_block() {
    fixture_over(expired_block(2, 1), |entry, settled_root| {
        receipt(Entry { prev_lane_blue_score: 7, ..entry }, settled_root)
    })
    .assert_proved_afresh();
}

/// A cached receipt whose journal does not decode proves nothing about the batch: the worker
/// proves the batch afresh instead of dying on the decode.
#[test]
fn cached_receipt_that_does_not_decode_is_proved_afresh() {
    fixture(|_, _| vec![0xAB; 3]).assert_proved_afresh();
}

/// A cached receipt that enters where the batch does is still reused: the worker publishes it
/// without proving, which is what the cache is for on a replay.
#[test]
fn cached_receipt_entering_where_the_batch_does_is_reused() {
    fixture(receipt).assert_reused();
}

/// The same for a block the lane re-anchors at: its receipt carries the re-anchoring, enters
/// where the batch does, and is reused.
#[test]
fn cached_receipt_of_a_block_the_lane_re_anchors_at_is_reused() {
    fixture_over(expired_block(2, 1), receipt).assert_reused();
}
