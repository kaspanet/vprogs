//! Pins the shutdown join against a hung prove: an attempt abandoned by its timeout leaves its
//! blocking task on the worker's single-threaded runtime, and dropping that runtime at exit
//! waits for every blocking task, so the worker shuts it down in the background instead. The
//! prover's shutdown join (which tears the GPU prover down before process exit) then completes
//! without waiting out the hang. Pre-fix, a hung batch prove (a lost CUDA request the timeout
//! abandoned) kept the join blocked for the life of the abandoned task.

// The backend trait returns `impl Future + 'static`, which an `async fn` cannot satisfy: its
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
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId, SchedulerTransaction};
use vprogs_l1_types::ChainBlockMetadata;
use vprogs_scheduling_scheduler::{ExecutionConfig, Processor, Scheduler, TransactionContext};
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_batch_prover::{BatchProver, BatchProverConfig};

/// Transaction-guest image id; only keys receipt-cache lookups, which this test never hits.
const TX_IMAGE_ID: [u8; 32] = [0u8; 32];

/// Batch-guest image id, keying the per-batch receipt cache.
const BATCH_IMAGE_ID: [u8; 32] = [1u8; 32];

/// Backend with the real attempt shape: the prove runs on the blocking pool under a timeout, so
/// a hanging prove is abandoned while its blocking task lingers on the worker's runtime.
/// Attempts are counted so the test can observe the abandoned retry.
#[derive(Clone)]
struct HangingBackend {
    /// Attempts run so far, shared with the worker's clone.
    attempts: Arc<AtomicUsize>,
}

impl vprogs_zk_transaction_prover::Backend for HangingBackend {
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

impl vprogs_zk_batch_prover::Backend for HangingBackend {
    fn prove_batch(
        &self,
        _inputs: &[u8],
        _receipts: Vec<Vec<u8>>,
    ) -> impl Future<Output = Result<Vec<u8>, String>> + Send + 'static {
        let attempts = self.attempts.clone();
        async move {
            attempts.fetch_add(1, Ordering::SeqCst);
            tokio::time::timeout(
                Duration::from_secs(1),
                tokio::task::spawn_blocking(|| {
                    thread::sleep(Duration::from_secs(120));
                    unreachable!("every attempt is abandoned long before it returns")
                }),
            )
            .await
            .map_err(|_| "prove timed out".to_owned())
            .and_then(|joined| joined.map_err(|err| format!("prove task failed: {err}")))
        }
    }

    fn journal_bytes(receipt: &Vec<u8>) -> Vec<u8> {
        receipt.clone()
    }

    fn batch_image_id(&self) -> &[u8; 32] {
        &BATCH_IMAGE_ID
    }
}

/// Processor publishing each transaction's artifact immediately, so its batch reaches the
/// worker's prove.
#[derive(Clone)]
struct PlainProcessor;

impl Processor<RocksDbStore> for PlainProcessor {
    fn process_transaction(
        &self,
        ctx: &mut TransactionContext<RocksDbStore, Self>,
    ) -> Result<(), Self::Error> {
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

/// The shutdown join completes while a prove attempt's abandoned blocking task still lingers on
/// the worker's runtime: the run loop exits on the latch, and the runtime is shut down in the
/// background rather than dropped. Pre-fix, the join waited out the abandoned task's hang,
/// keeping a restarting node from tearing its GPU prover down.
#[test]
fn shutdown_does_not_join_an_abandoned_hanging_prove() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(PlainProcessor),
            StorageConfig::default().with_store(storage.clone()),
        );

        let batch = scheduler.schedule(
            block(1, 0),
            vec![SchedulerTransaction::new(
                0,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                0,
            )],
        );
        batch.wait_committed_blocking();

        let attempts = Arc::new(AtomicUsize::new(0));
        let prover = BatchProver::new(
            HangingBackend { attempts: attempts.clone() },
            storage,
            BatchProverConfig {
                lane_key: Hash::default(),
                covenant_id: Hash::default(),
                deposit_spk_hash: [0u8; 32],
            },
        );
        prover.submit(&batch);
        wait_until("a hanging attempt to time out and retry", Duration::from_secs(10), || {
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
                "shutdown must not wait out the abandoned prove attempt",
            );
            thread::sleep(Duration::from_millis(20));
        }
        joiner.join().expect("the shutdown thread");

        scheduler.shutdown();
    }
}
