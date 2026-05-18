use tempfile::TempDir;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId};
use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler};
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::{DefaultConfig, RocksDbStore};
use vprogs_zk_backend_risc0_api::Backend;
use vprogs_zk_backend_risc0_covenant::ZkTag;
use vprogs_zk_backend_risc0_test_suite::{
    L1TransactionExt, batch_processor_elf, transaction_processor_elf,
};
use vprogs_zk_vm::{ProvingPipeline, Vm};

#[test]
fn test_zk_scheduler_e2e() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let backend =
        Backend::new(&transaction_processor_elf(), &batch_processor_elf(), ZkTag::R0Succinct);
    let mut scheduler = Scheduler::new(
        ExecutionConfig::default()
            .with_processor(Vm::new(backend.clone(), ProvingPipeline::transaction(backend))),
        StorageConfig::default().with_store(RocksDbStore::<DefaultConfig>::open(temp_dir.path())),
    );

    let batch = scheduler.schedule(
        ChainBlockMetadata::default(),
        vec![
            L1Transaction::for_l2_test(
                &[AccessMetadata::write(ResourceId::for_test(1))],
                &[1, 2, 3],
            )
            .into_scheduler_tx(0),
            L1Transaction::for_l2_test(
                &[AccessMetadata::write(ResourceId::for_test(2))],
                &[4, 5, 6],
            )
            .into_scheduler_tx(1),
        ],
    );

    batch.wait_tx_artifacts_published_blocking();

    scheduler.shutdown();
}
