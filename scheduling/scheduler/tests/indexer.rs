use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use tempfile::TempDir;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, AccessType, ChainSink, ResourceId, SchedulerTransaction};
use vprogs_scheduling_scheduler::{
    ExecutionConfig, Processor, ResourceIndexer, Scheduler, SchedulerState, TransactionContext,
};
use vprogs_scheduling_test_utils::SchedulerExt;
use vprogs_state_metadata::StateMetadata;
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::{StateSpace, Store, WriteBatch};

/// Toy indexer maintaining two indexes in `StateSpace::Index`:
/// - Event index (`0xAA` discriminator): append-style event entries keyed by `0xAA || version_be[8]
///   || resource_id[32]`.
/// - Snapshot index (`0xBB` discriminator): snapshot-style latest state keyed by `0xBB ||
///   resource_id[32]`, with value `version_be[8]`.
struct ToyIndexer;

impl ResourceIndexer for ToyIndexer {
    fn index_diff(
        &self,
        id: &ResourceId,
        _old: Option<&[u8]>,
        new: Option<&[u8]>,
        version: u64,
        wb: &mut dyn WriteBatch,
    ) {
        if new.is_some() {
            // Event index: append event for this version.
            let mut event_key = vec![0xaa];
            event_key.extend_from_slice(&version.to_be_bytes());
            event_key.extend_from_slice(id.as_slice());
            wb.put(StateSpace::Index, &event_key, b"");

            // Snapshot index: update snapshot to this version.
            let mut snapshot_key = vec![0xbb];
            snapshot_key.extend_from_slice(id.as_slice());
            wb.put(StateSpace::Index, &snapshot_key, &version.to_be_bytes());
        } else {
            // Snapshot index: deleted resource removes snapshot entry.
            let mut snapshot_key = vec![0xbb];
            snapshot_key.extend_from_slice(id.as_slice());
            wb.delete(StateSpace::Index, &snapshot_key);
        }
    }

    fn revert_diff(
        &self,
        id: &ResourceId,
        written: Option<&[u8]>,
        restored: Option<&[u8]>,
        reverted_version: u64,
        restored_version: u64,
        wb: &mut dyn WriteBatch,
    ) {
        // Event index: delete the entry written at the reverted version.
        if written.is_some() {
            let mut event_key = vec![0xaa];
            event_key.extend_from_slice(&reverted_version.to_be_bytes());
            event_key.extend_from_slice(id.as_slice());
            wb.delete(StateSpace::Index, &event_key);
        }

        // Snapshot index: restore the snapshot entry to the restored version, or delete if absent
        // before.
        let mut snapshot_key = vec![0xbb];
        snapshot_key.extend_from_slice(id.as_slice());
        if restored.is_some() {
            wb.put(StateSpace::Index, &snapshot_key, &restored_version.to_be_bytes());
        } else {
            wb.delete(StateSpace::Index, &snapshot_key);
        }
    }
}

/// Minimal non-restoring test processor for simulating forks at reused versions.
#[derive(Clone)]
struct TestForkProcessor;

impl<S: Store> Processor<S> for TestForkProcessor {
    fn process_transaction(
        &self,
        ctx: &mut TransactionContext<S, Self>,
    ) -> Result<(), Self::Error> {
        let tx_id = ctx.scheduler_tx().tx;
        for resource in ctx.resources_mut() {
            if resource.access_metadata().access_type == AccessType::Write {
                resource.data_mut().extend_from_slice(&tx_id.to_be_bytes());
            }
        }
        Ok(())
    }

    fn tx_image_id(&self) -> [u8; 32] {
        [0u8; 32]
    }

    fn batch_image_id(&self) -> [u8; 32] {
        [1u8; 32]
    }

    fn supports_restore(&self) -> bool {
        false
    }

    type Transaction = usize;
    type TransactionArtifact = Vec<u8>;
    type BatchArtifact = Vec<u8>;
    type AggregatorArtifact = Vec<u8>;
    type BatchMetadata = u64;
    type Error = ();
}

/// Waits until the batch at `index` has committed (last_committed reaches it on disk).
///
/// `ChainSink::append` returns only the batch id, so sink-driven tests synchronize on the
/// persisted commit pointer rather than a batch handle.
fn wait_last_committed(storage: &RocksDbStore, index: u64) {
    let start = Instant::now();
    loop {
        let last = StateMetadata::last_committed::<u64, _>(storage);
        if last.index() >= index {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timeout waiting for batch {index} to commit; last committed is {}",
            last.index()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn diff_feeds_both_indexes() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
    let state = SchedulerState::new(StorageConfig::default().with_store(storage.clone()));
    state.set_indexer(Arc::new(ToyIndexer));
    let mut scheduler =
        Scheduler::with_state(ExecutionConfig::default().with_processor(TestForkProcessor), state);

    let rid = ResourceId::for_test(1);
    let batch = scheduler
        .schedule(1, vec![SchedulerTransaction::new(10, vec![AccessMetadata::write(rid)], 0)]);
    batch.wait_committed_blocking();

    let event_entries: Vec<_> = storage.range_iter(StateSpace::Index, &[0xaa], &[0xab]).collect();
    assert_eq!(event_entries.len(), 1);
    let mut expected_event_key = vec![0xaa];
    expected_event_key.extend_from_slice(&1u64.to_be_bytes());
    expected_event_key.extend_from_slice(rid.as_slice());
    assert_eq!(event_entries[0].0, expected_event_key);

    let snapshot_entries: Vec<_> =
        storage.range_iter(StateSpace::Index, &[0xbb], &[0xbc]).collect();
    assert_eq!(snapshot_entries.len(), 1);
    let mut expected_snapshot_key = vec![0xbb];
    expected_snapshot_key.extend_from_slice(rid.as_slice());
    assert_eq!(snapshot_entries[0].0, expected_snapshot_key);
    assert_eq!(snapshot_entries[0].1, 1u64.to_be_bytes());

    scheduler.shutdown();
}

#[test]
fn unchanged_resource_writes_nothing() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
    let state = SchedulerState::new(StorageConfig::default().with_store(storage.clone()));
    state.set_indexer(Arc::new(ToyIndexer));
    let mut scheduler =
        Scheduler::with_state(ExecutionConfig::default().with_processor(TestForkProcessor), state);

    let rid = ResourceId::for_test(2);
    let batch = scheduler
        .schedule(1, vec![SchedulerTransaction::new(0, vec![AccessMetadata::read(rid)], 0)]);
    batch.wait_committed_blocking();

    let event_entries: Vec<_> = storage.range_iter(StateSpace::Index, &[0xaa], &[0xab]).collect();
    assert!(event_entries.is_empty(), "expected no event entries for read-only access");
    let snapshot_entries: Vec<_> =
        storage.range_iter(StateSpace::Index, &[0xbb], &[0xbc]).collect();
    assert!(snapshot_entries.is_empty(), "expected no snapshot entries for read-only access");

    scheduler.shutdown();
}

#[test]
fn revert_deletes_fork_entries_and_restores_snapshot() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
    let state = SchedulerState::new(StorageConfig::default().with_store(storage.clone()));
    state.set_indexer(Arc::new(ToyIndexer));
    let mut scheduler =
        Scheduler::with_state(ExecutionConfig::default().with_processor(TestForkProcessor), state);

    let rid = ResourceId::for_test(1);
    let batch1 = scheduler
        .schedule(1, vec![SchedulerTransaction::new(10, vec![AccessMetadata::write(rid)], 10)]);
    let batch2 = scheduler
        .schedule(2, vec![SchedulerTransaction::new(20, vec![AccessMetadata::write(rid)], 20)]);
    batch1.wait_committed_blocking();
    batch2.wait_committed_blocking();

    let event_entries_before: Vec<_> =
        storage.range_iter(StateSpace::Index, &[0xaa], &[0xab]).collect();
    assert_eq!(event_entries_before.len(), 2, "expected 2 event entries before rollback");

    let snapshot_entries_before: Vec<_> =
        storage.range_iter(StateSpace::Index, &[0xbb], &[0xbc]).collect();
    assert_eq!(snapshot_entries_before.len(), 1);
    assert_eq!(snapshot_entries_before[0].1, 2u64.to_be_bytes(), "expected snapshot at version 2");

    scheduler.rollback_to(1).expect("rollback should succeed");

    // Event index: version 2 entry must be deleted; version 1 remains.
    let event_entries_after: Vec<_> =
        storage.range_iter(StateSpace::Index, &[0xaa], &[0xab]).collect();
    assert_eq!(event_entries_after.len(), 1, "expected version 2 entry deleted");
    let mut expected_event_key_v1 = vec![0xaa];
    expected_event_key_v1.extend_from_slice(&1u64.to_be_bytes());
    expected_event_key_v1.extend_from_slice(rid.as_slice());
    assert_eq!(event_entries_after[0].0, expected_event_key_v1);

    // Snapshot index must be restored to version 1.
    let snapshot_entries_after: Vec<_> =
        storage.range_iter(StateSpace::Index, &[0xbb], &[0xbc]).collect();
    assert_eq!(snapshot_entries_after.len(), 1);
    assert_eq!(
        snapshot_entries_after[0].1,
        1u64.to_be_bytes(),
        "expected snapshot restored to version 1"
    );

    scheduler.shutdown();
}

#[test]
fn ghost_entries_reused_version_regression() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
    let state = SchedulerState::new(StorageConfig::default().with_store(storage.clone()));
    state.set_indexer(Arc::new(ToyIndexer));
    let mut scheduler =
        Scheduler::with_state(ExecutionConfig::default().with_processor(TestForkProcessor), state);

    let r1 = ResourceId::for_test(1);
    let r2 = ResourceId::for_test(2);

    // Fork 1: batch 1 writes r1, batch 2 writes r1.
    let b1 = scheduler
        .schedule(1, vec![SchedulerTransaction::new(10, vec![AccessMetadata::write(r1)], 10)]);
    let b2 = scheduler
        .schedule(2, vec![SchedulerTransaction::new(20, vec![AccessMetadata::write(r1)], 20)]);
    b1.wait_committed_blocking();
    b2.wait_committed_blocking();

    // Reorg: rollback to 1.
    scheduler.rollback_to(1).expect("rollback should succeed");

    // Fork 2: batch 2 writes r2 (instead of r1).
    let b2_fork = scheduler
        .schedule(2, vec![SchedulerTransaction::new(30, vec![AccessMetadata::write(r2)], 30)]);
    b2_fork.wait_committed_blocking();

    // Version 2 is canonical again in the oracle.
    let snapshot = storage.canonical_chain().snapshot();
    assert!(snapshot.is_canonical(2), "version 2 is canonical on winner fork");

    // Regression check: Fork 1's entry for r1 at version 2 must NOT exist in the event index.
    let mut stale_r1_v2_key = vec![0xaa];
    stale_r1_v2_key.extend_from_slice(&2u64.to_be_bytes());
    stale_r1_v2_key.extend_from_slice(r1.as_slice());
    assert_eq!(
        storage.get(StateSpace::Index, &stale_r1_v2_key),
        None,
        "rolled-back entry for r1 at reused version 2 must not exist"
    );

    // Fork 2's entry for r2 at version 2 must exist.
    let mut winner_r2_v2_key = vec![0xaa];
    winner_r2_v2_key.extend_from_slice(&2u64.to_be_bytes());
    winner_r2_v2_key.extend_from_slice(r2.as_slice());
    assert!(
        storage.get(StateSpace::Index, &winner_r2_v2_key).is_some(),
        "winner entry for r2 at version 2 must exist"
    );

    // Snapshot index: r1 is at version 1; r2 is at version 2.
    let mut snapshot_r1_key = vec![0xbb];
    snapshot_r1_key.extend_from_slice(r1.as_slice());
    assert_eq!(storage.get(StateSpace::Index, &snapshot_r1_key), Some(1u64.to_be_bytes().to_vec()));

    let mut snapshot_r2_key = vec![0xbb];
    snapshot_r2_key.extend_from_slice(r2.as_slice());
    assert_eq!(storage.get(StateSpace::Index, &snapshot_r2_key), Some(2u64.to_be_bytes().to_vec()));

    scheduler.shutdown();
}

#[test]
fn rollback_to_genesis_restores_none() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
    let state = SchedulerState::new(StorageConfig::default().with_store(storage.clone()));
    state.set_indexer(Arc::new(ToyIndexer));
    let mut scheduler =
        Scheduler::with_state(ExecutionConfig::default().with_processor(TestForkProcessor), state);

    let rid = ResourceId::for_test(4);
    let batch1 = scheduler
        .schedule(1, vec![SchedulerTransaction::new(10, vec![AccessMetadata::write(rid)], 10)]);
    batch1.wait_committed_blocking();

    scheduler.rollback_to(0).expect("rollback should succeed");

    let event_entries: Vec<_> = storage.range_iter(StateSpace::Index, &[0xaa], &[0xab]).collect();
    assert!(event_entries.is_empty(), "expected event index empty after genesis rollback");

    let snapshot_entries: Vec<_> =
        storage.range_iter(StateSpace::Index, &[0xbb], &[0xbc]).collect();
    assert!(snapshot_entries.is_empty(), "expected snapshot index empty after genesis rollback");

    scheduler.shutdown();
}

#[test]
fn restore_committed_re_derives_index_entries() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
    let state = SchedulerState::new(StorageConfig::default().with_store(storage.clone()));
    state.set_indexer(Arc::new(ToyIndexer));
    let mut scheduler = Scheduler::with_state(
        ExecutionConfig::default().with_processor(vprogs_scheduling_test_utils::Processor),
        state,
    );

    let r1 = ResourceId::for_test(1);
    let r2 = ResourceId::for_test(2);

    // Batch 1 (block metadata 100): writes r1.
    let b1 = scheduler
        .schedule(100, vec![SchedulerTransaction::new(10, vec![AccessMetadata::write(r1)], 10)]);
    b1.wait_committed_blocking();

    // Batch 2 (block metadata 200): updates r1 and writes new resource r2.
    let b2 = scheduler.schedule(
        200,
        vec![
            SchedulerTransaction::new(20, vec![AccessMetadata::write(r1)], 20),
            SchedulerTransaction::new(30, vec![AccessMetadata::write(r2)], 30),
        ],
    );
    b2.wait_committed_blocking();

    // Verify index entries are present before rollback.
    let mut r1_v2_event_key = vec![0xaa];
    r1_v2_event_key.extend_from_slice(&2u64.to_be_bytes());
    r1_v2_event_key.extend_from_slice(r1.as_slice());
    assert!(storage.get(StateSpace::Index, &r1_v2_event_key).is_some());

    let mut r2_v2_event_key = vec![0xaa];
    r2_v2_event_key.extend_from_slice(&2u64.to_be_bytes());
    r2_v2_event_key.extend_from_slice(r2.as_slice());
    assert!(storage.get(StateSpace::Index, &r2_v2_event_key).is_some());

    let mut r1_snapshot_key = vec![0xbb];
    r1_snapshot_key.extend_from_slice(r1.as_slice());
    assert_eq!(storage.get(StateSpace::Index, &r1_snapshot_key), Some(2u64.to_be_bytes().to_vec()));

    let mut r2_snapshot_key = vec![0xbb];
    r2_snapshot_key.extend_from_slice(r2.as_slice());
    assert_eq!(storage.get(StateSpace::Index, &r2_snapshot_key), Some(2u64.to_be_bytes().to_vec()));

    // Reorg: rollback to batch 1.
    scheduler.rollback_to(1).expect("rollback should succeed");

    // Entries for version 2 are reverted.
    assert_eq!(storage.get(StateSpace::Index, &r1_v2_event_key), None);
    assert_eq!(storage.get(StateSpace::Index, &r2_v2_event_key), None);
    assert_eq!(storage.get(StateSpace::Index, &r1_snapshot_key), Some(1u64.to_be_bytes().to_vec()));
    assert_eq!(storage.get(StateSpace::Index, &r2_snapshot_key), None);

    // Re-reorg: the same block (metadata 200) returns and is restored, not re-executed.
    let b2_restored = scheduler.schedule(
        200,
        vec![
            SchedulerTransaction::new(20, vec![AccessMetadata::write(r1)], 20),
            SchedulerTransaction::new(30, vec![AccessMetadata::write(r2)], 30),
        ],
    );
    assert!(b2_restored.restored(), "returning block must follow the restore path");
    b2_restored.wait_committed_blocking();

    // Index entries must be re-derived and present again.
    assert!(
        storage.get(StateSpace::Index, &r1_v2_event_key).is_some(),
        "event entry for r1 in restored batch must be re-derived"
    );
    assert!(
        storage.get(StateSpace::Index, &r2_v2_event_key).is_some(),
        "event entry for r2 in restored batch must be re-derived"
    );
    assert_eq!(
        storage.get(StateSpace::Index, &r1_snapshot_key),
        Some(2u64.to_be_bytes().to_vec()),
        "snapshot entry for r1 in restored batch must be updated to version 2"
    );
    assert_eq!(
        storage.get(StateSpace::Index, &r2_snapshot_key),
        Some(2u64.to_be_bytes().to_vec()),
        "snapshot entry for r2 in restored batch must be re-derived"
    );

    scheduler.shutdown();
}

/// Drives a full reorg cycle through the `ChainSink` surface only: chain A grows, a competing
/// fork B replaces it past a rollback, then chain A's blocks return through the restore path.
/// The event and snapshot indexes must match each stage exactly.
#[test]
fn chain_sink_reorg_competing_forks_and_returning_blocks() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
    let state = SchedulerState::new(StorageConfig::default().with_store(storage.clone()));
    state.set_indexer(Arc::new(ToyIndexer));
    let mut scheduler = Scheduler::with_state(
        ExecutionConfig::default().with_processor(vprogs_scheduling_test_utils::Processor),
        state,
    );

    let r1 = ResourceId::for_test(1);
    let r2 = ResourceId::for_test(2);

    let event_entry = |version: u64, id: &ResourceId| {
        let mut key = vec![0xaa];
        key.extend_from_slice(&version.to_be_bytes());
        key.extend_from_slice(id.as_slice());
        (key, Vec::new())
    };
    let snapshot_entry = |id: &ResourceId, version: u64| {
        let mut key = vec![0xbb];
        key.extend_from_slice(id.as_slice());
        (key, version.to_be_bytes().to_vec())
    };
    let event_entries = || -> Vec<(Vec<u8>, Vec<u8>)> {
        storage.range_iter(StateSpace::Index, &[0xaa], &[0xab]).collect()
    };
    let snapshot_entries = || -> Vec<(Vec<u8>, Vec<u8>)> {
        storage.range_iter(StateSpace::Index, &[0xbb], &[0xbc]).collect()
    };

    // Stage 1: chain A grows. Block 10 creates r1 (id 1), block 11 rewrites it (id 2),
    // block 12 rewrites it again and creates r2 (id 3).
    scheduler.append(10, vec![SchedulerTransaction::new(0, vec![AccessMetadata::write(r1)], 10)]);
    scheduler.append(11, vec![SchedulerTransaction::new(0, vec![AccessMetadata::write(r1)], 11)]);
    scheduler.append(
        12,
        vec![
            SchedulerTransaction::new(0, vec![AccessMetadata::write(r1)], 12),
            SchedulerTransaction::new(1, vec![AccessMetadata::write(r2)], 13),
        ],
    );
    wait_last_committed(&storage, 3);
    assert_eq!(scheduler.tip(), 3);

    assert_eq!(
        event_entries(),
        vec![event_entry(1, &r1), event_entry(2, &r1), event_entry(3, &r1), event_entry(3, &r2)]
    );
    assert_eq!(snapshot_entries(), vec![snapshot_entry(&r1, 3), snapshot_entry(&r2, 3)]);

    // Stage 2: reorg at the split point (id 1). The multi-version walk reverts r1's rewrites
    // (v3 -> v2 -> v1, inverse diffs composing) and r2's creation.
    scheduler.rollback(1);
    assert_eq!(scheduler.tip(), 1);
    let oracle = storage.canonical_chain().snapshot();
    assert!(oracle.is_canonical(1));
    assert!(!oracle.is_canonical(2), "orphaned fork A version must not be canonical");
    assert!(!oracle.is_canonical(3), "orphaned fork A version must not be canonical");

    assert_eq!(event_entries(), vec![event_entry(1, &r1)]);
    assert_eq!(snapshot_entries(), vec![snapshot_entry(&r1, 1)]);

    // Stage 3: fork B extends the split point with unseen blocks; their ids continue past the
    // orphans (never reused), leaving a canonical gap at 2 and 3.
    let b1 = scheduler
        .append(20, vec![SchedulerTransaction::new(0, vec![AccessMetadata::write(r2)], 20)]);
    let b2 = scheduler
        .append(21, vec![SchedulerTransaction::new(0, vec![AccessMetadata::write(r1)], 21)]);
    assert_eq!(b1, 4, "new blocks continue past orphaned ids");
    assert_eq!(b2, 5, "new blocks continue past orphaned ids");
    wait_last_committed(&storage, 5);
    assert_eq!(scheduler.tip(), 5);
    let oracle = storage.canonical_chain().snapshot();
    assert!(oracle.is_canonical(4) && oracle.is_canonical(5));
    assert!(!oracle.is_canonical(2) && !oracle.is_canonical(3));

    assert_eq!(
        event_entries(),
        vec![event_entry(1, &r1), event_entry(4, &r2), event_entry(5, &r1)]
    );
    assert_eq!(snapshot_entries(), vec![snapshot_entry(&r1, 5), snapshot_entry(&r2, 4)]);

    // Stage 4: reorg back to the split point; fork B's committed batches revert (the walk
    // hits 5 then 4, skipping the already-orphaned 3 and 2), reproducing stage 2 exactly.
    scheduler.rollback(1);
    assert_eq!(scheduler.tip(), 1);
    let oracle = storage.canonical_chain().snapshot();
    assert!(!oracle.is_canonical(4), "orphaned fork B version must not be canonical");
    assert!(!oracle.is_canonical(5), "orphaned fork B version must not be canonical");

    assert_eq!(event_entries(), vec![event_entry(1, &r1)]);
    assert_eq!(snapshot_entries(), vec![snapshot_entry(&r1, 1)]);

    // Stage 5: chain A returns. The same block hashes reuse ids 2 and 3, restore from committed
    // disk state (ignoring the 999 payloads), and re-derive their index entries.
    let a2 = scheduler
        .append(11, vec![SchedulerTransaction::new(0, vec![AccessMetadata::write(r1)], 999)]);
    assert_eq!(a2, 2, "returning block reuses its id");
    wait_last_committed(&storage, 2);
    scheduler.assert_written_state(r1, vec![10, 11]);

    let a3 = scheduler.append(
        12,
        vec![
            SchedulerTransaction::new(0, vec![AccessMetadata::write(r1)], 999),
            SchedulerTransaction::new(1, vec![AccessMetadata::write(r2)], 999),
        ],
    );
    assert_eq!(a3, 3, "returning block reuses its id");
    wait_last_committed(&storage, 3);
    assert_eq!(scheduler.tip(), 3);
    let oracle = storage.canonical_chain().snapshot();
    assert!(oracle.is_canonical(2) && oracle.is_canonical(3));
    assert!(!oracle.is_canonical(4) && !oracle.is_canonical(5));

    assert_eq!(
        event_entries(),
        vec![event_entry(1, &r1), event_entry(2, &r1), event_entry(3, &r1), event_entry(3, &r2)]
    );
    assert_eq!(snapshot_entries(), vec![snapshot_entry(&r1, 3), snapshot_entry(&r2, 3)]);

    // The restored chain's state matches the original chain A, not the 999 re-execution.
    scheduler.assert_written_state(r1, vec![10, 11, 12]);
    scheduler.assert_written_state(r2, vec![13]);

    scheduler.shutdown();
}

#[test]
fn indexer_double_apply_is_idempotent() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
    let indexer = ToyIndexer;

    let rid = ResourceId::for_test(3);
    let mut wb = storage.write_batch();
    indexer.index_diff(&rid, None, Some(b"data"), 1, &mut wb);
    indexer.index_diff(&rid, None, Some(b"data"), 1, &mut wb);
    storage.commit(wb);

    let event_entries: Vec<_> = storage.range_iter(StateSpace::Index, &[0xaa], &[0xab]).collect();
    assert_eq!(event_entries.len(), 1);
    let snapshot_entries: Vec<_> =
        storage.range_iter(StateSpace::Index, &[0xbb], &[0xbc]).collect();
    assert_eq!(snapshot_entries.len(), 1);
}
