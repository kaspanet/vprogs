use std::sync::Arc;

use tempfile::TempDir;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, AccessType, ResourceId, SchedulerTransaction};
use vprogs_scheduling_scheduler::{
    ExecutionConfig, Processor, ResourceIndexer, Scheduler, SchedulerState, TransactionContext,
};
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::{StateSpace, Store, WriteBatch};

/// Toy indexer maintaining two indexes in `StateSpace::Index`:
/// - Index A (`0xAA` discriminator): append-style event entries keyed by `0xAA || version_be[8] ||
///   resource_id[32]`.
/// - Index B (`0xBB` discriminator): snapshot-style latest state keyed by `0xBB ||
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
            // Index A: append event for this version.
            let mut key_a = vec![0xaa];
            key_a.extend_from_slice(&version.to_be_bytes());
            key_a.extend_from_slice(id.as_slice());
            wb.put(StateSpace::Index, &key_a, b"");

            // Index B: update snapshot to this version.
            let mut key_b = vec![0xbb];
            key_b.extend_from_slice(id.as_slice());
            wb.put(StateSpace::Index, &key_b, &version.to_be_bytes());
        } else {
            // Index B: deleted resource removes snapshot entry.
            let mut key_b = vec![0xbb];
            key_b.extend_from_slice(id.as_slice());
            wb.delete(StateSpace::Index, &key_b);
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
        // Index A: delete the entry written at the reverted version.
        if written.is_some() {
            let mut key_a = vec![0xaa];
            key_a.extend_from_slice(&reverted_version.to_be_bytes());
            key_a.extend_from_slice(id.as_slice());
            wb.delete(StateSpace::Index, &key_a);
        }

        // Index B: restore the snapshot entry to the restored version, or delete if absent before.
        let mut key_b = vec![0xbb];
        key_b.extend_from_slice(id.as_slice());
        if restored.is_some() {
            wb.put(StateSpace::Index, &key_b, &restored_version.to_be_bytes());
        } else {
            wb.delete(StateSpace::Index, &key_b);
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

    let a_entries: Vec<_> = storage.range_iter(StateSpace::Index, &[0xaa], &[0xab]).collect();
    assert_eq!(a_entries.len(), 1);
    let mut expected_a = vec![0xaa];
    expected_a.extend_from_slice(&1u64.to_be_bytes());
    expected_a.extend_from_slice(rid.as_slice());
    assert_eq!(a_entries[0].0, expected_a);

    let b_entries: Vec<_> = storage.range_iter(StateSpace::Index, &[0xbb], &[0xbc]).collect();
    assert_eq!(b_entries.len(), 1);
    let mut expected_b = vec![0xbb];
    expected_b.extend_from_slice(rid.as_slice());
    assert_eq!(b_entries[0].0, expected_b);
    assert_eq!(b_entries[0].1, 1u64.to_be_bytes());

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

    let a_entries: Vec<_> = storage.range_iter(StateSpace::Index, &[0xaa], &[0xab]).collect();
    assert!(a_entries.is_empty(), "expected no index A entries for read-only access");
    let b_entries: Vec<_> = storage.range_iter(StateSpace::Index, &[0xbb], &[0xbc]).collect();
    assert!(b_entries.is_empty(), "expected no index B entries for read-only access");

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

    let a_entries_before: Vec<_> =
        storage.range_iter(StateSpace::Index, &[0xaa], &[0xab]).collect();
    assert_eq!(a_entries_before.len(), 2, "expected 2 index A entries before rollback");

    let b_entries_before: Vec<_> =
        storage.range_iter(StateSpace::Index, &[0xbb], &[0xbc]).collect();
    assert_eq!(b_entries_before.len(), 1);
    assert_eq!(b_entries_before[0].1, 2u64.to_be_bytes(), "expected snapshot at version 2");

    scheduler.rollback_to(1).expect("rollback should succeed");

    // Index A: version 2 entry must be deleted; version 1 remains.
    let a_entries_after: Vec<_> = storage.range_iter(StateSpace::Index, &[0xaa], &[0xab]).collect();
    assert_eq!(a_entries_after.len(), 1, "expected version 2 entry deleted");
    let mut expected_a1 = vec![0xaa];
    expected_a1.extend_from_slice(&1u64.to_be_bytes());
    expected_a1.extend_from_slice(rid.as_slice());
    assert_eq!(a_entries_after[0].0, expected_a1);

    // Index B: snapshot must be restored to version 1.
    let b_entries_after: Vec<_> = storage.range_iter(StateSpace::Index, &[0xbb], &[0xbc]).collect();
    assert_eq!(b_entries_after.len(), 1);
    assert_eq!(b_entries_after[0].1, 1u64.to_be_bytes(), "expected snapshot restored to version 1");

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

    // Regression check: Fork 1's entry for r1 at version 2 must NOT exist in index A.
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

    // Index B: r1 is at version 1; r2 is at version 2.
    let mut b_r1_key = vec![0xbb];
    b_r1_key.extend_from_slice(r1.as_slice());
    assert_eq!(storage.get(StateSpace::Index, &b_r1_key), Some(1u64.to_be_bytes().to_vec()));

    let mut b_r2_key = vec![0xbb];
    b_r2_key.extend_from_slice(r2.as_slice());
    assert_eq!(storage.get(StateSpace::Index, &b_r2_key), Some(2u64.to_be_bytes().to_vec()));

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

    let a_entries: Vec<_> = storage.range_iter(StateSpace::Index, &[0xaa], &[0xab]).collect();
    assert!(a_entries.is_empty(), "expected index A empty after genesis rollback");

    let b_entries: Vec<_> = storage.range_iter(StateSpace::Index, &[0xbb], &[0xbc]).collect();
    assert!(b_entries.is_empty(), "expected index B empty after genesis rollback");

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
    let mut r1_v2_key_a = vec![0xaa];
    r1_v2_key_a.extend_from_slice(&2u64.to_be_bytes());
    r1_v2_key_a.extend_from_slice(r1.as_slice());
    assert!(storage.get(StateSpace::Index, &r1_v2_key_a).is_some());

    let mut r2_v2_key_a = vec![0xaa];
    r2_v2_key_a.extend_from_slice(&2u64.to_be_bytes());
    r2_v2_key_a.extend_from_slice(r2.as_slice());
    assert!(storage.get(StateSpace::Index, &r2_v2_key_a).is_some());

    let mut r1_key_b = vec![0xbb];
    r1_key_b.extend_from_slice(r1.as_slice());
    assert_eq!(storage.get(StateSpace::Index, &r1_key_b), Some(2u64.to_be_bytes().to_vec()));

    let mut r2_key_b = vec![0xbb];
    r2_key_b.extend_from_slice(r2.as_slice());
    assert_eq!(storage.get(StateSpace::Index, &r2_key_b), Some(2u64.to_be_bytes().to_vec()));

    // Reorg: rollback to batch 1.
    scheduler.rollback_to(1).expect("rollback should succeed");

    // Entries for version 2 are reverted.
    assert_eq!(storage.get(StateSpace::Index, &r1_v2_key_a), None);
    assert_eq!(storage.get(StateSpace::Index, &r2_v2_key_a), None);
    assert_eq!(storage.get(StateSpace::Index, &r1_key_b), Some(1u64.to_be_bytes().to_vec()));
    assert_eq!(storage.get(StateSpace::Index, &r2_key_b), None);

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
        storage.get(StateSpace::Index, &r1_v2_key_a).is_some(),
        "index A entry for r1 in restored batch must be re-derived"
    );
    assert!(
        storage.get(StateSpace::Index, &r2_v2_key_a).is_some(),
        "index A entry for r2 in restored batch must be re-derived"
    );
    assert_eq!(
        storage.get(StateSpace::Index, &r1_key_b),
        Some(2u64.to_be_bytes().to_vec()),
        "index B snapshot entry for r1 in restored batch must be updated to version 2"
    );
    assert_eq!(
        storage.get(StateSpace::Index, &r2_key_b),
        Some(2u64.to_be_bytes().to_vec()),
        "index B snapshot entry for r2 in restored batch must be re-derived"
    );

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

    let a_entries: Vec<_> = storage.range_iter(StateSpace::Index, &[0xaa], &[0xab]).collect();
    assert_eq!(a_entries.len(), 1);
    let b_entries: Vec<_> = storage.range_iter(StateSpace::Index, &[0xbb], &[0xbc]).collect();
    assert_eq!(b_entries.len(), 1);
}
