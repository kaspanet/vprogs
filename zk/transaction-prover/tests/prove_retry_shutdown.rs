//! Pins the transaction prover's retry ownership: a prove attempt is fallible and returns its
//! error to the worker's loop, which retries under the shutdown latch. A persistent failure
//! keeps the worker escapable, so the prover's shutdown join (which tears the GPU prover down
//! before process exit) completes instead of waiting on an infinite retry inside the backend.

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
use vprogs_scheduling_scheduler::{
    ExecutionConfig, Processor, ScheduledBatch, ScheduledTransaction, Scheduler, TransactionContext,
};
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_transaction_prover::TransactionProver;

/// Transaction-guest image id; only keys receipt-cache lookups, which these tests never hit.
const TX_IMAGE_ID: [u8; 32] = [0u8; 32];

/// Receipt every successful attempt returns.
const RECEIPT: &[u8] = &[7];

/// Backend whose attempts fail while their count is below `succeed_at`, counting every call so a
/// test can observe the retry.
#[derive(Clone)]
struct FlakyBackend {
    /// Attempts run so far, shared with the worker's clone.
    attempts: Arc<AtomicUsize>,
    /// The one-based attempt count the first success happens at.
    succeed_at: usize,
}

impl vprogs_zk_transaction_prover::Backend for FlakyBackend {
    type Receipt = Vec<u8>;

    fn image_id(&self) -> &[u8; 32] {
        &TX_IMAGE_ID
    }

    fn prove_transaction(
        &self,
        _input_bytes: Vec<u8>,
    ) -> impl Future<Output = Result<Self::Receipt, String>> + Send + 'static {
        let attempts = self.attempts.clone();
        let succeed_at = self.succeed_at;
        async move {
            if attempts.fetch_add(1, Ordering::SeqCst) + 1 < succeed_at {
                Err("synthetic prove failure".to_owned())
            } else {
                Ok(RECEIPT.to_vec())
            }
        }
    }
}

/// Processor executing every transaction without publishing its artifact: the prover's worker is
/// the publisher these tests observe.
#[derive(Clone)]
struct PlainProcessor;

impl Processor<RocksDbStore> for PlainProcessor {
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
        [1u8; 32]
    }

    type Transaction = usize;
    type TransactionArtifact = Vec<u8>;
    type BatchArtifact = Vec<u8>;
    type AggregatorArtifact = Vec<u8>;
    type BatchMetadata = ChainBlockMetadata;
    type Error = ();
}

/// Commits a one-transaction batch and returns it with its transaction.
fn committed_tx(
    scheduler: &mut Scheduler<RocksDbStore, PlainProcessor>,
) -> (
    ScheduledBatch<RocksDbStore, PlainProcessor>,
    ScheduledTransaction<RocksDbStore, PlainProcessor>,
) {
    let batch = scheduler.schedule(
        ChainBlockMetadata { hash: Hash::from_bytes([1; 32]), parent_id: 0, ..Default::default() },
        vec![SchedulerTransaction::new(0, vec![AccessMetadata::write(ResourceId::for_test(1))], 0)],
    );
    batch.wait_committed_blocking();
    let tx = batch.txs()[0].clone();
    (batch, tx)
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

/// A failed attempt hands its error to the worker's loop, which retries the identical inputs and
/// publishes the first verified receipt as the transaction's artifact. The attempt count pins
/// the retry: exactly the failures, then the one success.
#[test]
fn a_failing_sequence_retries_until_success() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(PlainProcessor),
            StorageConfig::default().with_store(storage.clone()),
        );
        let (batch, tx) = committed_tx(&mut scheduler);
        let backend = FlakyBackend { attempts: Arc::new(AtomicUsize::new(0)), succeed_at: 3 };
        let prover = TransactionProver::new(backend.clone());
        prover.submit(&tx, Vec::new());

        batch.wait_tx_artifacts_published_blocking();
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 3, "two failures, then the success");
        assert_eq!(*tx.artifact(), RECEIPT, "the verified receipt is the published artifact");

        prover.shutdown();
        scheduler.shutdown();
    }
}

/// A persistent failure keeps retrying under the shutdown latch, so the prover's shutdown join
/// completes instead of waiting forever on the retry. Pre-fix, the backend's own infinite retry
/// wedged the join, keeping a restarting node from tearing its GPU prover down.
#[test]
fn shutdown_exits_a_failing_prove_retry_loop() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(PlainProcessor),
            StorageConfig::default().with_store(storage.clone()),
        );
        let (_batch, tx) = committed_tx(&mut scheduler);
        let backend =
            FlakyBackend { attempts: Arc::new(AtomicUsize::new(0)), succeed_at: usize::MAX };
        let prover = TransactionProver::new(backend.clone());
        prover.submit(&tx, Vec::new());
        wait_until("the failing loop to retry at least twice", Duration::from_secs(10), || {
            backend.attempts.load(Ordering::SeqCst) >= 2
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

        scheduler.shutdown();
    }
}
