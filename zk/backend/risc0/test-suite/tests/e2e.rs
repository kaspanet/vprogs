use tempfile::TempDir;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId, SchedulerTransaction};
use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler};
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_backend_risc0_api::Backend;
use vprogs_zk_backend_risc0_test_suite::{batch_processor_elf, transaction_processor_elf};
use vprogs_zk_vm::{ProvingPipeline, Vm};

#[test]
fn test_zk_scheduler_e2e() {
    let transaction_elf = transaction_processor_elf();
    let batch_elf = batch_processor_elf();

    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());

    let backend = Backend::new(&transaction_elf, &batch_elf);
    let vm = Vm::new(backend, ProvingPipeline::None);
    let mut scheduler = Scheduler::new(
        ExecutionConfig::default().with_processor(vm),
        StorageConfig::default().with_store(storage),
    );

    let mut kaspa_tx1 = L1Transaction::default();
    kaspa_tx1.version = 1;
    kaspa_tx1.payload = vec![1, 2, 3];
    let mut kaspa_tx2 = L1Transaction::default();
    kaspa_tx2.version = 1;
    kaspa_tx2.payload = vec![4, 5, 6];

    let batch = scheduler.schedule(
        ChainBlockMetadata::default(),
        vec![
            SchedulerTransaction::new(
                0,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                kaspa_tx1,
            ),
            SchedulerTransaction::new(
                1,
                vec![AccessMetadata::write(ResourceId::for_test(2))],
                kaspa_tx2,
            ),
        ],
    );

    batch.wait_committed_blocking();

    scheduler.shutdown();
}
