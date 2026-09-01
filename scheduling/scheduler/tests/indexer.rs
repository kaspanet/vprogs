use std::sync::Arc;

use tempfile::TempDir;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId, SchedulerTransaction};
use vprogs_scheduling_scheduler::{ExecutionConfig, ResourceIndexer, Scheduler, SchedulerState};
use vprogs_scheduling_test_utils::Processor;
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::{StateSpace, Store, WriteBatch};

struct ToyIndexer;

impl ResourceIndexer for ToyIndexer {
    fn index_events(
        &self,
        id: &ResourceId,
        _old: Option<&[u8]>,
        _new: Option<&[u8]>,
        version: u64,
        wb: &mut dyn WriteBatch,
    ) {
        let mut key = b"events__".to_vec();
        key.extend_from_slice(id.as_slice());
        wb.put(StateSpace::Index, &key, &version.to_be_bytes());
    }

    fn index_state(
        &self,
        id: &ResourceId,
        old: Option<&[u8]>,
        new: Option<&[u8]>,
        _version: u64,
        wb: &mut dyn WriteBatch,
    ) {
        let old_bucket = old.map_or(0u8, |d| *d.first().unwrap_or(&0));
        let new_bucket = new.map_or(0u8, |d| *d.first().unwrap_or(&0));
        let mut key = b"state___".to_vec();
        key.push(old_bucket);
        key.push(new_bucket);
        key.extend_from_slice(id.as_slice());
        wb.put(StateSpace::Index, &key, b"1");
    }

    fn revert_state(
        &self,
        id: &ResourceId,
        restored: Option<&[u8]>,
        _version: u64,
        wb: &mut dyn WriteBatch,
    ) {
        let mut key = b"revert__".to_vec();
        key.extend_from_slice(id.as_slice());
        if let Some(restored) = restored {
            key.extend_from_slice(restored);
        }
        wb.put(StateSpace::Index, &key, b"1");
    }
}

#[test]
fn diff_writes_feed_indexer() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
    let state = SchedulerState::new(StorageConfig::default().with_store(storage.clone()));
    state.set_indexer(Arc::new(ToyIndexer));
    let mut scheduler =
        Scheduler::with_state(ExecutionConfig::default().with_processor(Processor), state);

    let rid = ResourceId::for_test(1);
    let batch = scheduler
        .schedule(1, vec![SchedulerTransaction::new(0, vec![AccessMetadata::write(rid)], 0)]);
    batch.wait_committed_blocking();

    let evt_entries: Vec<_> = storage.prefix_iter(StateSpace::Index, b"events__").collect();
    assert_eq!(evt_entries.len(), 1);
    assert_eq!(evt_entries[0].0, [b"events__".as_slice(), rid.as_slice()].concat());

    let st_entries: Vec<_> = storage.prefix_iter(StateSpace::Index, b"state___").collect();
    assert_eq!(st_entries.len(), 1);

    scheduler.shutdown();
}

#[test]
fn unchanged_resource_writes_nothing() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
    let state = SchedulerState::new(StorageConfig::default().with_store(storage.clone()));
    state.set_indexer(Arc::new(ToyIndexer));
    let mut scheduler =
        Scheduler::with_state(ExecutionConfig::default().with_processor(Processor), state);

    let rid = ResourceId::for_test(2);
    let batch = scheduler
        .schedule(1, vec![SchedulerTransaction::new(0, vec![AccessMetadata::read(rid)], 0)]);
    batch.wait_committed_blocking();

    let evt_entries: Vec<_> = storage.prefix_iter(StateSpace::Index, b"events__").collect();
    assert!(evt_entries.is_empty(), "expected no event entries for read-only access");
    let st_entries: Vec<_> = storage.prefix_iter(StateSpace::Index, b"state___").collect();
    assert!(st_entries.is_empty(), "expected no state entries for read-only access");

    scheduler.shutdown();
}

#[test]
fn indexer_double_apply_is_idempotent() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
    let indexer = ToyIndexer;

    let rid = ResourceId::for_test(3);
    let mut wb = storage.write_batch();
    indexer.index_events(&rid, None, Some(b"data"), 1, &mut wb);
    indexer.index_events(&rid, None, Some(b"data"), 1, &mut wb);
    storage.commit(wb);

    let evt_entries: Vec<_> = storage.prefix_iter(StateSpace::Index, b"events__").collect();
    assert_eq!(evt_entries.len(), 1);
}
