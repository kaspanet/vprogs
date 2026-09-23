//! Reproduction: a restored batch resolves a read-only access through the disk latest pointer
//! instead of the in-batch `prev` chain, so it can forward a value the preceding batch has already
//! superseded.

use std::sync::{Arc, Condvar, Mutex};

use tempfile::TempDir;
use vprogs_core_smt::{Key, Node, Tree, WriteBatch as SmtWriteBatch};
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId, SchedulerTransaction};
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler};
use vprogs_scheduling_test_utils::Processor;
use vprogs_state_version::StateVersion;
use vprogs_storage_canonical_chain::CanonicalChain;
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::{PrefixIterator, StateSpace, Store};

/// A latch that holds a caller while closed.
#[derive(Default)]
struct Gate {
    /// True while callers of [`wait`](Self::wait) are held.
    closed: Mutex<bool>,
    /// Signalled when the gate opens.
    opened: Condvar,
}

impl Gate {
    /// Closes the gate; subsequent [`wait`](Self::wait) callers block until [`open`](Self::open).
    fn close(&self) {
        *self.closed.lock().expect("gate poisoned") = true;
    }

    /// Opens the gate and releases every held caller.
    fn open(&self) {
        *self.closed.lock().expect("gate poisoned") = false;
        self.opened.notify_all();
    }

    /// Blocks while the gate is closed.
    fn wait(&self) {
        let mut closed = self.closed.lock().expect("gate poisoned");
        while *closed {
            closed = self.opened.wait(closed).expect("gate poisoned");
        }
    }
}

/// A [`Store`] whose commits are held while its gate is closed. Every other operation delegates
/// unchanged, so reads issued against a closed gate observe a store that provably does not yet
/// carry any held commit.
#[derive(Clone)]
struct GatedStore {
    /// The store every operation delegates to.
    inner: RocksDbStore,
    /// Held commits wait here.
    gate: Arc<Gate>,
}

impl Store for GatedStore {
    type WriteBatch = <RocksDbStore as Store>::WriteBatch;

    fn get(&self, state_space: StateSpace, key: &[u8]) -> Option<Vec<u8>> {
        self.inner.get(state_space, key)
    }

    fn write_batch(&self) -> Self::WriteBatch {
        self.inner.write_batch()
    }

    fn commit(&self, write_batch: Self::WriteBatch) {
        self.gate.wait();
        self.inner.commit(write_batch);
    }

    fn prefix_iter(&self, state_space: StateSpace, prefix: &[u8]) -> PrefixIterator<'_> {
        self.inner.prefix_iter(state_space, prefix)
    }

    fn canonical_chain(&self) -> CanonicalChain {
        self.inner.canonical_chain()
    }

    fn prefix_iter_rev(&self, state_space: StateSpace, prefix: &[u8]) -> PrefixIterator<'_> {
        self.inner.prefix_iter_rev(state_space, prefix)
    }

    fn range_iter(&self, state_space: StateSpace, start: &[u8], end: &[u8]) -> PrefixIterator<'_> {
        self.inner.range_iter(state_space, start, end)
    }
}

impl Tree for GatedStore {
    type Hasher = <RocksDbStore as Tree>::Hasher;
    type Snapshot = <RocksDbStore as Tree>::Snapshot;

    fn snapshot(&self) -> Self::Snapshot {
        self.inner.snapshot()
    }

    fn node(&self, key: &Key, max_version: u64, snapshot: &Self::Snapshot) -> Option<(u64, Node)> {
        self.inner.node(key, max_version, snapshot)
    }

    fn prune(&self, wb: &mut impl SmtWriteBatch, version: u64) {
        self.inner.prune(wb, version)
    }
}

/// Repro: a restored batch's read-only access resolves against the disk latest pointer, not the
/// batch that precedes it, and forwards the stale value to the next batch.
///
/// Block 1 writes the resource and block 2 only reads it. After both are reorged away, the
/// resource's latest pointer is deleted and both blocks are re-appended and restored. Restoring
/// block 2 takes the read-only branch, which resolves the access with
/// `StateVersion::from_latest_data`, the disk pointer, rather than block 1's restored written state
/// reachable through the access's `prev`. Only block 1's commit re-points that pointer, and the
/// gate holds it, so the read resolves against the pre-block-1 state and block 2 forwards an empty
/// resource to block 3.
///
/// The interleaving is not a race: the gate is closed before either restore is scheduled and opens
/// only after every value under test has been read, so block 1's commit cannot land inside the
/// window. Restores are pure reads and are never held.
#[test]
fn test_restored_read_only_access_forwards_stale_disk_value() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let gate = Arc::new(Gate::default());
    {
        let storage =
            GatedStore { inner: RocksDbStore::open(temp_dir.path()), gate: Arc::clone(&gate) };
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        // Block 1 writes 100 to the resource; block 2 declares it read-only, so block 2 writes no
        // rollback pointer for it. Waiting on block 2 implies block 1 committed: commits are
        // submitted in batch order.
        scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                0,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                100,
            )],
        );
        scheduler
            .schedule(
                2,
                vec![SchedulerTransaction::new(
                    0,
                    vec![AccessMetadata::read(ResourceId::for_test(1))],
                    200,
                )],
            )
            .wait_committed_blocking();

        // Reorg both away. Block 1's rollback pointer records that the resource did not exist
        // before it, so the rollback deletes the latest pointer entirely.
        scheduler.rollback_to(0).expect("rollback should succeed");
        assert!(
            scheduler
                .state()
                .storage()
                .store()
                .get(StateSpace::StatePtrLatest, &ResourceId::for_test(1)[..])
                .is_none(),
            "the rollback must delete the latest pointer"
        );

        // Hold every commit from here on. Block 1's restore can no longer re-point the latest
        // pointer, which is the write block 2's restore read would need to observe.
        gate.close();

        let restored1 = scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                0,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                100,
            )],
        );
        let restored2 = scheduler.schedule(
            2,
            vec![SchedulerTransaction::new(
                0,
                vec![AccessMetadata::read(ResourceId::for_test(1))],
                200,
            )],
        );

        // Restores are pure reads, so both resolve while the gate holds every write.
        restored1.wait_processed_blocking();
        restored2.wait_processed_blocking();
        assert!(restored1.restored() && restored2.restored(), "both blocks must restore");

        // Block 3 writes the resource again, chaining off block 2's forwarded written state.
        let following = scheduler.schedule(
            3,
            vec![SchedulerTransaction::new(
                0,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                300,
            )],
        );
        following.wait_processed_blocking();

        // Every value under test has been read; releasing the gate now only lets the commits land.
        gate.open();
        following.wait_committed_blocking();

        // Block 2 is read-only, so it must forward block 1's restored 100 and block 3 must append
        // to it. Reading the disk pointer instead yields the pre-block-1 empty resource, and block
        // 3 appends to nothing.
        let store = scheduler.state().storage().store();
        let state = StateVersion::from_latest_data(store.as_ref(), ResourceId::for_test(1));
        let expected: Vec<u8> = [100usize, 300].iter().flat_map(|id| id.to_be_bytes()).collect();
        assert_eq!(
            *state.data(),
            expected,
            "block 2's read-only restore must forward block 1's 100, not the pre-rollback state"
        );

        scheduler.shutdown();
    }
}
