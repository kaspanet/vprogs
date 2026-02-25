use tempfile::TempDir;
use vprogs_core_types::AccessType;
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler};
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_risc0_backend::Backend;
use vprogs_zk_vm::{AccessMetadata, BatchMetadata, ResourceId, Transaction, Vm};

/// Loads the pre-built transaction processor ELF from the repository.
fn transaction_processor_elf() -> Vec<u8> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let elf_path =
        format!("{manifest_dir}/../../zk-risc-0/transaction-processor/compiled/program.elf");
    std::fs::read(&elf_path).unwrap_or_else(|e| {
        panic!(
            "transaction processor ELF not found at {elf_path}: {e}\n\
             Run `./zk-risc-0/build-guests.sh transaction-processor` to rebuild it."
        )
    })
}

/// Loads the pre-built batch processor ELF from the repository.
fn batch_processor_elf() -> Vec<u8> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let elf_path = format!("{manifest_dir}/../../zk-risc-0/batch-processor/compiled/program.elf");
    std::fs::read(&elf_path).unwrap_or_else(|e| {
        panic!(
            "batch processor ELF not found at {elf_path}: {e}\n\
             Run `./zk-risc-0/build-guests.sh batch-processor` to rebuild it."
        )
    })
}

#[test]
fn test_zk_scheduler_e2e() {
    let transaction_elf = transaction_processor_elf();
    let batch_elf = batch_processor_elf();

    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());

    let backend = Backend::new(transaction_elf, batch_elf);
    let vm = Vm::new(backend);
    let mut scheduler = Scheduler::new(
        ExecutionConfig::default().with_vm(vm),
        StorageConfig::default().with_store(storage),
    );

    let batch = scheduler.schedule(
        BatchMetadata { batch_index: 1 },
        vec![
            Transaction {
                tx_bytes: vec![1, 2, 3],
                accesses: vec![AccessMetadata {
                    id: ResourceId(vec![0, 1]),
                    access_type: AccessType::Write,
                }],
            },
            Transaction {
                tx_bytes: vec![4, 5, 6],
                accesses: vec![AccessMetadata {
                    id: ResourceId(vec![0, 2]),
                    access_type: AccessType::Write,
                }],
            },
        ],
    );

    batch.wait_committed_blocking();

    scheduler.shutdown();
}
