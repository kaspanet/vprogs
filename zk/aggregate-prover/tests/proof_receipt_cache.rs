//! Round-trips proof-receipt blobs through the storage read/write workers via the
//! [`ScheduledBatch`] and [`ScheduledBundle`] handles, covering the cache-hit path the dev-mode
//! proving sims don't reach (they only ever miss-then-store, never replay a fork to read a stored
//! receipt back).

use kaspa_hashes::Hash;
use tempfile::TempDir;
use vprogs_core_types::BatchMetadata;
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler};
use vprogs_scheduling_test_utils::Processor;
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_aggregate_prover::{BundleBlocks, ScheduledBundle};

#[test]
fn proof_receipt_round_trips_through_storage_workers() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
    let mut scheduler = Scheduler::new(
        ExecutionConfig::default().with_processor(Processor),
        StorageConfig::default().with_store(storage),
    );

    // An empty batch yields a live handle whose storage manager runs both workers; the receipt
    // column family is independent of the batch's own (empty) state. The batch derives its own
    // per-batch receipt key from its checkpoint coordinate and the processor's image id.
    let batch = scheduler.schedule(1, vec![]);

    // A single-batch bundle sharing the batch's start coordinate; the batch is its storage gateway
    // for the aggregate receipt.
    let block = Hash::from_bytes(batch.checkpoint().metadata().block_hash());
    let bundle: ScheduledBundle<()> = ScheduledBundle::new(
        1,
        batch.checkpoint().index(),
        BundleBlocks { from_block: block, block_prove_to: block },
        None,
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");
    rt.block_on(async {
        let batch_receipt = vec![9u8; 64];

        // Cache miss before anything is stored.
        assert!(batch.read_batch_receipt().resolve().await.is_none());

        // Store the receipt, wait for the write worker to commit it, then read it back: the
        // read worker serves the exact receipt (the flip-reorg acceleration path).
        batch.write_batch_receipt(batch_receipt.clone()).wait().await;
        assert_eq!(batch.read_batch_receipt().resolve().await, Some(batch_receipt));

        // The aggregate receipt at the same coordinate keys differently (it is a distinct kind), so
        // it misses despite the stored batch receipt, then round-trips on its own key.
        let agg_receipt = vec![7u8; 32];
        assert!(bundle.read_agg_receipt(&batch, [0u8; 32]).resolve().await.is_none());
        bundle.write_agg_receipt(&batch, [0u8; 32], agg_receipt.clone()).wait().await;
        assert_eq!(bundle.read_agg_receipt(&batch, [0u8; 32]).resolve().await, Some(agg_receipt));
    });

    scheduler.shutdown();
}
