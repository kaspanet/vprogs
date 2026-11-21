//! Pins that a long run of reads of one resource is resolved and freed without growing the
//! stack.
//!
//! The accesses of a resource form a chain across the transactions of a batch and across the
//! batches in flight. Three things used to go along that chain one nested call per access, and
//! a long enough run of reads overflowed the stack of the thread that did it, which aborts the
//! process:
//!
//! - Handing a resolved state down a run of reads, by recursion.
//! - Freeing a run of reads that was scheduled behind an access that had its state already: every
//!   such read stayed linked from the access before it, so an early batch that outlived the later
//!   ones held all their accesses and freed them from inside its own drop.
//! - Freeing a restored batch: its accesses never resolve a read state, which is what releases the
//!   link to the access before, so they stayed linked back to the first one.

use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use tempfile::TempDir;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, AccessType, ResourceId, SchedulerTransaction};
use vprogs_scheduling_scheduler::{ExecutionConfig, Processor, Scheduler, TransactionContext};
use vprogs_storage_manager::{ReadConfig, StorageConfig};
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::Store;

/// The id of the one transaction that writes, which [`ReadCheckProcessor`] holds at its gate.
const WRITE_TX: usize = 0;

/// What the write leaves in the resource, and every read after it has to see.
const WRITTEN: &[u8] = b"written";

/// Reads in the run. In a debug build the recursion overflowed a thread's default stack
/// between 3,000 and 4,000 reads.
const READS: usize = 8_000;

/// Batches the reads are spread over.
const BATCHES: usize = 50;

/// Reads in a chain that is freed on a small stack.
const CHAIN: usize = 1_000;

/// The stack of the thread that frees a batch: plenty for a drop that frees what the batch
/// itself holds, a fraction of what a nested drop per access of [`CHAIN`] needs.
const SMALL_STACK: usize = 64 * 1024;

/// A gate that holds its callers while closed, and tells whether one is held.
#[derive(Default)]
struct Gate {
    /// Whether the gate is closed, and how many callers are inside [`wait`](Self::wait).
    state: Mutex<(bool, usize)>,
    /// Signalled when the gate opens and when a caller arrives.
    changed: Condvar,
}

impl Gate {
    /// Closes the gate; subsequent [`wait`](Self::wait) callers block until [`open`](Self::open).
    fn close(&self) {
        self.state.lock().expect("gate poisoned").0 = true;
    }

    /// Opens the gate and releases every held caller.
    fn open(&self) {
        self.state.lock().expect("gate poisoned").0 = false;
        self.changed.notify_all();
    }

    /// Blocks while the gate is closed.
    fn wait(&self) {
        let mut state = self.state.lock().expect("gate poisoned");
        state.1 += 1;
        self.changed.notify_all();
        while state.0 {
            state = self.changed.wait(state).expect("gate poisoned");
        }
        state.1 -= 1;
    }

    /// Waits until a caller is held at the closed gate, and returns whether one is.
    fn wait_held(&self) -> bool {
        let state = self.state.lock().expect("gate poisoned");
        let held = |state: &(bool, usize)| state.0 && state.1 > 0;
        let (state, _) = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(30), |state| !held(state))
            .expect("gate poisoned");
        held(&state)
    }
}

/// A processor whose write leaves [`WRITTEN`] in its resource, after passing the gate, and
/// that counts the reads that see exactly that.
#[derive(Clone)]
struct ReadCheckProcessor {
    /// Holds [`WRITE_TX`] while closed, which keeps every read behind it unresolved.
    gate: Arc<Gate>,
    /// The reads that saw [`WRITTEN`].
    correct_reads: Arc<AtomicUsize>,
}

impl<S: Store> Processor<S> for ReadCheckProcessor {
    fn process_transaction(
        &self,
        ctx: &mut TransactionContext<S, Self>,
    ) -> Result<(), Self::Error> {
        if ctx.scheduler_tx().tx == WRITE_TX {
            self.gate.wait();
        }
        for resource in ctx.resources_mut() {
            match resource.access_metadata().access_type {
                AccessType::Write => resource.set_data(WRITTEN.to_vec()),
                AccessType::Read if resource.data() == WRITTEN => {
                    self.correct_reads.fetch_add(1, Ordering::Relaxed);
                }
                AccessType::Read => {}
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
        true
    }

    type Transaction = usize;
    type TransactionArtifact = Vec<u8>;
    type BatchArtifact = Vec<u8>;
    type AggregatorArtifact = Vec<u8>;
    type BatchMetadata = u64;
    type Error = ();
}

/// The resource every transaction of these tests accesses.
fn resource() -> ResourceId {
    ResourceId::for_test(1)
}

/// The transaction that writes the resource.
fn write() -> SchedulerTransaction<usize> {
    SchedulerTransaction::new(0, vec![AccessMetadata::write(resource())], WRITE_TX)
}

/// `count` transactions that read the resource, with ids from `first` on.
fn reads(first: usize, count: usize) -> Vec<SchedulerTransaction<usize>> {
    (first..first + count)
        .map(|tx| SchedulerTransaction::new(tx as u32, vec![AccessMetadata::read(resource())], tx))
        .collect()
}

/// The scheduler under test.
type TestScheduler = Scheduler<RocksDbStore, ReadCheckProcessor>;

/// A scheduler over a fresh store, with the gate and the counter of its processor.
///
/// With `single_worker` it has one execution worker and one read worker. A worker that parks
/// while it still holds a batch or a read keeps their accesses alive, and with several workers
/// that could be what frees a chain, on its own large stack, after the drop under test.
fn open(temp_dir: &TempDir, single_worker: bool) -> (TestScheduler, Arc<Gate>, Arc<AtomicUsize>) {
    let (gate, correct_reads) = (Arc::new(Gate::default()), Arc::new(AtomicUsize::new(0)));
    let processor = ReadCheckProcessor { gate: gate.clone(), correct_reads: correct_reads.clone() };
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
    let mut execution_config = ExecutionConfig::default().with_processor(processor);
    let mut storage_config = StorageConfig::default().with_store(storage);
    if single_worker {
        execution_config = execution_config.with_worker_count(1);
        storage_config =
            storage_config.with_read_config(ReadConfig::default().with_worker_count(1));
    }
    (Scheduler::new(execution_config, storage_config), gate, correct_reads)
}

/// Drops `value` on a thread with a stack of [`SMALL_STACK`].
fn drop_on_a_small_stack<T: Send + 'static>(value: T) {
    thread::Builder::new()
        .stack_size(SMALL_STACK)
        .spawn(move || drop(value))
        .expect("failed to spawn the dropping thread")
        .join()
        .expect("the drop panicked");
}

/// Tests that a run of reads that spans the transactions of a batch and many batches in
/// flight is resolved, and that every read sees what the write before it left.
///
/// The write is held while it executes, so every read scheduled after it waits for its state:
/// the reads of its own batch and of the batches after it. Once it is released, its state is
/// handed down the whole run in one go, on the thread that executed the write.
#[test]
fn a_long_run_of_reads_behind_a_write_is_resolved() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let (mut scheduler, gate, correct_reads) = open(&temp_dir, false);

    // The first batch holds the write and the first reads, the batches after it only reads.
    gate.close();
    let per_batch = READS / BATCHES;
    let mut txs = vec![write()];
    txs.extend(reads(1, per_batch));
    let mut last = scheduler.schedule(1, txs);
    assert!(gate.wait_held(), "the write never started");
    for batch in 1..BATCHES {
        last = scheduler.schedule(batch as u64 + 1, reads(1 + batch * per_batch, per_batch));
    }

    gate.open();
    last.wait_committed_blocking();

    assert_eq!(correct_reads.load(Ordering::Relaxed), READS, "not every read saw the write");
    scheduler.shutdown();
}

/// Tests that a batch which outlives a run of reads scheduled after it does not hold their
/// accesses.
///
/// The write has committed when the reads are scheduled, so each read finds the state of the
/// access before it at once. Each of them used to stay linked from that access all the same.
/// The first batch then held every read after it, and dropping it freed them all, one from
/// inside the drop of the other.
#[test]
fn a_batch_that_outlives_a_run_of_reads_does_not_hold_them() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let (mut scheduler, _gate, correct_reads) = open(&temp_dir, true);

    let first = scheduler.schedule(1, vec![write()]);
    first.wait_committed_blocking();
    scheduler.schedule(2, reads(1, CHAIN)).wait_committed_blocking();

    // One more batch makes the scheduler let go of the reads' batch.
    scheduler.schedule(3, reads(1 + CHAIN, 1)).wait_committed_blocking();
    drop_on_a_small_stack(first);

    assert_eq!(correct_reads.load(Ordering::Relaxed), CHAIN + 1, "not every read saw the write");
    scheduler.shutdown();
}

/// Tests that the accesses of a restored batch do not stay linked to each other.
///
/// A block with a write and a run of reads commits, is rolled back and returns, so its batch
/// is restored from disk. Nothing of a restored batch executes, and its accesses used to keep
/// the link to the access before them for good: dropping the batch freed the whole run from
/// its last access, one from inside the drop of the other.
#[test]
fn a_restored_batch_does_not_keep_its_accesses_linked() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let (mut scheduler, _gate, correct_reads) = open(&temp_dir, true);

    let mut block = vec![write()];
    block.extend(reads(1, CHAIN));
    scheduler.schedule(1, block.clone()).wait_committed_blocking();
    scheduler.rollback_to(0).expect("rollback to genesis");

    let restored = scheduler.schedule(1, block);
    assert!(restored.restored(), "the returning block must be restored from disk");
    restored.wait_committed_blocking();

    // One more batch reads the restored state and makes the scheduler let go of the batch.
    scheduler.schedule(2, reads(1 + CHAIN, 1)).wait_committed_blocking();
    drop_on_a_small_stack(restored);

    assert_eq!(correct_reads.load(Ordering::Relaxed), CHAIN + 1, "not every read saw the write");
    scheduler.shutdown();
}
