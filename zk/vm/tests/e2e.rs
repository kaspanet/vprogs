use tempfile::TempDir;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId, SchedulerTransaction};
use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler};
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_backend_risc0_api::Backend;
use vprogs_zk_vm::{ProvingPipeline, Vm};

/// Loads the pre-built transaction processor ELF from the repository.
fn transaction_processor_elf() -> Vec<u8> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let elf_path =
        format!("{manifest_dir}/../backend/risc0/transaction-processor/compiled/program.elf");
    std::fs::read(&elf_path).unwrap_or_else(|e| {
        panic!(
            "transaction processor ELF not found at {elf_path}: {e}\n\
             Run `./zk/backend/risc0/build-guests.sh transaction-processor` to rebuild it."
        )
    })
}

/// Loads the pre-built batch processor ELF from the repository.
fn batch_processor_elf() -> Vec<u8> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let elf_path = format!("{manifest_dir}/../backend/risc0/batch-processor/compiled/program.elf");
    std::fs::read(&elf_path).unwrap_or_else(|e| {
        panic!(
            "batch processor ELF not found at {elf_path}: {e}\n\
             Run `./zk/backend/risc0/build-guests.sh batch-processor` to rebuild it."
        )
    })
}

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
    kaspa_tx1.payload = vec![1, 2, 3];
    let mut kaspa_tx2 = L1Transaction::default();
    kaspa_tx2.payload = vec![4, 5, 6];

    let batch = scheduler.schedule(
        ChainBlockMetadata::default(),
        vec![
            SchedulerTransaction::new(
                kaspa_tx1,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
            ),
            SchedulerTransaction::new(
                kaspa_tx2,
                vec![AccessMetadata::write(ResourceId::for_test(2))],
            ),
        ],
    );

    batch.wait_committed_blocking();

    scheduler.shutdown();
}
