//! Reproduces the batch worker's settled-root compare firing with no divergence when the batch
//! it examined was canceled by a rollback or never committed because of a shutdown.
//!
//! The compare ran after a `select` whose shutdown arm also resolves before the commit, and
//! `wait_committed` resolves on cancellation as well, so a reorg that canceled a batch while its
//! proof was in flight (or orphaned it right after the commit), or a shutdown before the commit,
//! compared the receipt's settled root against the checkpoint's pre-batch root: a guaranteed
//! mismatch, so the assert killed the worker thread and nothing proved afterwards.
//!
//! Both tests feed the real worker canned batch receipts whose settled root can never equal any
//! root the fixture's store holds, so any execution that reaches the compare fails it. The
//! worker now compares only a batch that committed and stayed canonical, so the canned receipt
//! is never compared and the worker survives to serve the batch that follows.

// The backend traits return `impl Future + 'static`, which an `async fn` cannot satisfy: its
// future borrows `&self`.
#![allow(clippy::manual_async_fn)]

use std::{
    future::Future,
    panic,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use kaspa_hashes::Hash;
use tempfile::TempDir;
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId, SchedulerTransaction};
use vprogs_l1_types::ChainBlockMetadata;
use vprogs_scheduling_scheduler::{
    ExecutionConfig, Processor, ScheduledBatch, Scheduler, TransactionContext,
};
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_abi::batch_processor::{BatchTransition, BatchTransitionArgs};
use vprogs_zk_batch_prover::{BatchProver, BatchProverConfig};

/// Transaction payload whose execution parks inside the processor until `release` opens. The
/// transaction's artifact is published before parking, so its batch proves while its commit
/// stays pending.
const GATE_TX: usize = 100;

/// Transaction-guest image id; only keys receipt-cache lookups, which these tests never hit.
const TX_IMAGE_ID: [u8; 32] = [0u8; 32];

/// Batch-guest image id, keying the per-batch receipt cache.
const BATCH_IMAGE_ID: [u8; 32] = [1u8; 32];

/// Settled `new_state` every canned receipt carries. It equals no root the fixture's store can
/// hold, so a worker that reaches the root compare with one of these receipts fails it.
const DIVERGENT_ROOT: [u8; 32] = [7u8; 32];

/// A batch receipt whose journal settles [`DIVERGENT_ROOT`].
fn canned_receipt() -> Vec<u8> {
    let mut journal = Vec::new();
    BatchTransition::encode(
        &mut journal,
        BatchTransitionArgs {
            prev_state: &[0u8; 32],
            prev_lane_tip: &Hash::default(),
            prev_lane_blue_score: 0,
            new_state: &DIVERGENT_ROOT,
            new_lane_tip: &Hash::default(),
            new_lane_blue_score: 0,
            lane_key: &Hash::default(),
            covenant_id: &[0u8; 32],
            tx_image_id: &TX_IMAGE_ID,
            deposit_spk_hash: &[0u8; 32],
            lane_expired: false,
            exits: &[],
        },
    );
    journal
}

/// Backend handing out canned batch receipts. The first prove optionally parks until the test
/// opens its release gate, signalling entry first, so a rollback or shutdown can land while the
/// proof is in flight.
#[derive(Clone)]
struct CannedBackend {
    /// Entry signal and release gate for the parked first prove, if any.
    prove_park: Option<(Arc<AtomicAsyncLatch>, Arc<AtomicAsyncLatch>)>,
}

impl vprogs_zk_transaction_prover::Backend for CannedBackend {
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

impl vprogs_zk_batch_prover::Backend for CannedBackend {
    fn prove_batch(
        &self,
        _inputs: &[u8],
        _receipts: Vec<Vec<u8>>,
    ) -> impl Future<Output = Result<Vec<u8>, String>> + Send + 'static {
        let park = self.prove_park.clone();
        async move {
            if let Some((entered, release)) = park {
                entered.open();
                release.wait_blocking();
            }
            Ok(canned_receipt())
        }
    }

    fn journal_bytes(receipt: &Vec<u8>) -> Vec<u8> {
        receipt.clone()
    }

    fn batch_image_id(&self) -> &[u8; 32] {
        &BATCH_IMAGE_ID
    }
}

/// Backend whose batch attempts always fail, counting every call so a test can observe the
/// retry.
#[derive(Clone)]
struct FailingBackend {
    /// Attempts run so far, shared with the worker's clone.
    attempts: Arc<AtomicUsize>,
}

impl vprogs_zk_transaction_prover::Backend for FailingBackend {
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

impl vprogs_zk_batch_prover::Backend for FailingBackend {
    fn prove_batch(
        &self,
        _inputs: &[u8],
        _receipts: Vec<Vec<u8>>,
    ) -> impl Future<Output = Result<Vec<u8>, String>> + Send + 'static {
        let attempts = self.attempts.clone();
        async move {
            attempts.fetch_add(1, Ordering::SeqCst);
            Err("synthetic prove failure".to_owned())
        }
    }

    fn journal_bytes(receipt: &Vec<u8>) -> Vec<u8> {
        receipt.clone()
    }

    fn batch_image_id(&self) -> &[u8; 32] {
        &BATCH_IMAGE_ID
    }
}

/// Processor that publishes each transaction's artifact immediately and parks [`GATE_TX`]
/// inside execution, holding its batch's commit back until the test releases it.
#[derive(Clone)]
struct GatedProcessor {
    /// Opened by the test to let a gated transaction finish.
    release: Arc<AtomicAsyncLatch>,
}

impl Processor<RocksDbStore> for GatedProcessor {
    fn process_transaction(
        &self,
        ctx: &mut TransactionContext<RocksDbStore, Self>,
    ) -> Result<(), Self::Error> {
        ctx.scheduled_tx().publish_artifact(Some(Vec::new()));
        if ctx.scheduler_tx().tx == GATE_TX {
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

/// Chain-block metadata whose only distinguishing fields are the block hash and its parent id.
fn block(hash: u8, parent_id: u64) -> ChainBlockMetadata {
    ChainBlockMetadata { hash: Hash::from_bytes([hash; 32]), parent_id, ..Default::default() }
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

/// A persistent batch-prove failure keeps retrying under the shutdown latch, so the prover's
/// shutdown join completes instead of waiting forever on the retry. Pre-fix, the backend's own
/// infinite retry wedged the join, keeping a restarting node from tearing its GPU prover down.
#[test]
fn shutdown_exits_a_failing_prove_retry_loop() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let release = Arc::new(AtomicAsyncLatch::new());
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(GatedProcessor { release: release.clone() }),
            StorageConfig::default().with_store(storage.clone()),
        );

        // A committed anchor below the victim.
        let anchor = scheduler.schedule(block(1, 0), vec![]);
        anchor.wait_committed_blocking();

        // The victim commits and publishes its tx artifacts, so the worker reaches its batch
        // prove, which fails on every attempt.
        let victim = scheduler.schedule(
            block(2, 1),
            vec![SchedulerTransaction::new(
                0,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                0,
            )],
        );
        victim.wait_committed_blocking();
        let attempts = Arc::new(AtomicUsize::new(0));
        let prover = BatchProver::new(
            FailingBackend { attempts: attempts.clone() },
            storage,
            BatchProverConfig {
                lane_key: Hash::default(),
                covenant_id: Hash::default(),
                deposit_spk_hash: [0u8; 32],
            },
        );
        prover.submit(&victim);
        wait_until("the failing loop to retry at least twice", Duration::from_secs(10), || {
            attempts.load(Ordering::SeqCst) >= 2
        });

        let done = Arc::new(AtomicAsyncLatch::new());
        let joiner = {
            let prover = prover.clone();
            let done = done.clone();
            thread::spawn(move || {
                prover.shutdown();
                done.open();
            })
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done.is_open() {
            assert!(
                Instant::now() < deadline,
                "shutdown must exit the failing retry loop promptly",
            );
            thread::sleep(Duration::from_millis(20));
        }
        joiner.join().expect("the shutdown thread");

        release.open();
        scheduler.shutdown();
    }
}

/// Polls `batch.artifact_published()` until it opens, failing with `msg` after five seconds. A
/// non-empty batch's artifact latch opens only when the worker publishes its receipt, so this is
/// the worker-is-alive signal.
fn wait_published(batch: &ScheduledBatch<RocksDbStore, GatedProcessor>, msg: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !batch.artifact_published() {
        assert!(Instant::now() < deadline, "{msg}");
        thread::sleep(Duration::from_millis(20));
    }
}

/// A batch canceled by a rollback while its proof is in flight must not trip the root compare.
///
/// The worker parks inside proving, the rollback cancels the committed batch and rolls the store
/// back under it, then the prove finishes. Pre-fix, the compare read the checkpoint's rolled-back
/// root, mismatched the canned settled root, and killed the worker, so the batch submitted behind
/// the victim never published an artifact.
#[test]
fn canceled_batch_skips_the_root_compare() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let release = Arc::new(AtomicAsyncLatch::new());
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(GatedProcessor { release: release.clone() }),
            StorageConfig::default().with_store(storage.clone()),
        );

        // A committed anchor below the victim: it gives the rollback a target.
        let anchor = scheduler.schedule(block(1, 0), vec![]);
        anchor.wait_committed_blocking();

        // The victim executes and commits normally, then parks inside its proof.
        let victim = scheduler.schedule(
            block(2, 1),
            vec![SchedulerTransaction::new(
                0,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                0,
            )],
        );
        victim.wait_committed_blocking();

        let prove_entered = Arc::new(AtomicAsyncLatch::new());
        let prove_release = Arc::new(AtomicAsyncLatch::new());
        let prover = BatchProver::new(
            CannedBackend { prove_park: Some((prove_entered.clone(), prove_release.clone())) },
            storage.clone(),
            BatchProverConfig {
                lane_key: Hash::default(),
                covenant_id: Hash::default(),
                deposit_spk_hash: [0u8; 32],
            },
        );
        prover.submit(&victim);
        prove_entered.wait_blocking();

        // Cancel the victim while its proof is in flight; the store rolls its write back.
        scheduler.rollback_to(1).expect("rollback should succeed");
        assert!(victim.canceled(), "the in-flight batch should be canceled");

        // Finishing the proof publishes the victim's canned receipt; the compare must be
        // skipped rather than fired against the rolled-back root.
        prove_release.open();

        // The probe parks inside execution, so its commit stays pending and its own compare
        // never runs: its artifact publishing proves a live worker processed it.
        let probe = scheduler.schedule(
            block(3, 1),
            vec![SchedulerTransaction::new(
                0,
                vec![AccessMetadata::write(ResourceId::for_test(2))],
                GATE_TX,
            )],
        );
        prover.submit(&probe);
        wait_published(&probe, "the worker must survive the canceled batch and keep proving");

        prover.shutdown();
        release.open();
        scheduler.shutdown();
    }
}

/// A shutdown that lands while the worker waits for an uncommitted batch must not trip the
/// compare: the checkpoint's root is still the pre-batch one, with no divergence. The false
/// assert would kill the worker inside `shutdown`'s join, so its panic is observed through the
/// process panic hook (nextest runs each test in its own process, so the hook counts only this
/// test's panics).
#[test]
fn shutdown_before_commit_skips_the_root_compare() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let release = Arc::new(AtomicAsyncLatch::new());
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(GatedProcessor { release: release.clone() }),
            StorageConfig::default().with_store(storage.clone()),
        );

        // A committed anchor below the victim.
        let anchor = scheduler.schedule(block(1, 0), vec![]);
        anchor.wait_committed_blocking();

        // The victim's transaction publishes its artifact then parks inside execution, so the
        // batch never commits: the worker proves it, publishes its artifact, and parks on the
        // commit wait.
        let victim = scheduler.schedule(
            block(2, 1),
            vec![SchedulerTransaction::new(
                0,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                GATE_TX,
            )],
        );
        let prover = BatchProver::new(
            CannedBackend { prove_park: None },
            storage.clone(),
            BatchProverConfig {
                lane_key: Hash::default(),
                covenant_id: Hash::default(),
                deposit_spk_hash: [0u8; 32],
            },
        );
        prover.submit(&victim);
        wait_published(&victim, "the worker must prove the batch and publish its artifact");

        let panics = Arc::new(AtomicUsize::new(0));
        let previous_hook = panic::take_hook();
        panic::set_hook(Box::new({
            let panics = panics.clone();
            move |_| {
                panics.fetch_add(1, Ordering::SeqCst);
            }
        }));
        prover.shutdown();
        assert_eq!(panics.load(Ordering::SeqCst), 0, "shutdown must not trip the root compare");
        panic::set_hook(previous_hook);

        release.open();
        scheduler.shutdown();
    }
}
