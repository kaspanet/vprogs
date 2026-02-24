use tempfile::TempDir;
use vprogs_core_types::AccessType;
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler};
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_risc0_backend::Risc0MockBackend;
use vprogs_zk_vm::{ZkAccessMetadata, ZkBatchMetadata, ZkResourceId, ZkTransaction, ZkVm};

/// Returns the default guest ELF embedded by `zk-risc-0/backend` at build time.
///
/// With the RISC-0 toolchain installed, `zk-risc-0/backend/build.rs` cross-compiles
/// `zk-risc-0/guest` and embeds the ELF. Without it, returns an empty vec and the test is skipped.
fn guest_elf() -> Vec<u8> {
  vprogs_zk_risc0_backend::GUEST_ELF.to_vec()
}

#[test]
#[ignore = "requires RISC-0 toolchain to build guest ELF"]
fn test_zk_scheduler_e2e() {
  let elf = guest_elf();
  if elf.is_empty() {
    eprintln!(
      "skipping: no guest ELF available (install RISC-0 toolchain and rebuild zk-risc-0/backend)"
    );
    return;
  }

  let temp_dir = TempDir::new().expect("failed to create temp dir");
  let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());

  let vm = ZkVm::new(Risc0MockBackend, elf);
  let mut scheduler = Scheduler::new(
    ExecutionConfig::default().with_vm(vm),
    StorageConfig::default().with_store(storage),
  );

  let batch = scheduler.schedule(
    ZkBatchMetadata { batch_index: 1 },
    vec![
      ZkTransaction {
        tx_bytes: vec![1, 2, 3],
        accesses: vec![ZkAccessMetadata {
          id: ZkResourceId(vec![0, 1]),
          access_type: AccessType::Write,
        }],
      },
      ZkTransaction {
        tx_bytes: vec![4, 5, 6],
        accesses: vec![ZkAccessMetadata {
          id: ZkResourceId(vec![0, 2]),
          access_type: AccessType::Write,
        }],
      },
    ],
  );

  batch.wait_committed_blocking();

  // Verify each transaction produced effects with non-empty journal bytes.
  for tx in batch.txs() {
    let effects = tx.effects();
    assert!(!effects.journal_bytes.is_empty(), "expected non-empty journal bytes for tx");
  }

  scheduler.shutdown();
}
