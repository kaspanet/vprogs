use borsh::BorshDeserialize;
use tempfile::TempDir;
use vprogs_core_types::AccessType;
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler};
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_risc0_backend::Risc0Prover;
use vprogs_zk_types::Journal;
use vprogs_zk_vm::{ZkAccessMetadata, ZkBatchMetadata, ZkResourceId, ZkTransaction, ZkVm};

/// Loads the pre-built guest ELF from the repository.
///
/// The ELF is committed at `zk-risc-0/guest/compiled/guest.elf` and can be rebuilt
/// with `./zk-risc-0/build-guests.sh guest`.
fn guest_elf() -> Vec<u8> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let elf_path = format!("{manifest_dir}/../../zk-risc-0/guest/compiled/program.elf");
    std::fs::read(&elf_path).unwrap_or_else(|e| {
        panic!(
            "guest ELF not found at {elf_path}: {e}\n\
             Run `./zk-risc-0/build-guests.sh guest` to rebuild it."
        )
    })
}

#[test]
fn test_zk_scheduler_e2e() {
    let elf = guest_elf();

    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());

    let vm = ZkVm::new(Risc0Prover, elf);
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

    // The guest calls `compute_output_commitment(&[])` — empty ops, so ops_hash
    // is the BLAKE3 digest of an empty sequence.
    let expected_ops_hash: [u8; 32] = blake3::Hasher::new().finalize().into();

    for (i, tx) in batch.txs().iter().enumerate() {
        let effects = tx.effects();
        assert!(!effects.journal_bytes.is_empty(), "expected non-empty journal bytes for tx {i}");

        let journal =
            Journal::try_from_slice(&effects.journal_bytes).expect("failed to deserialize journal");

        assert_eq!(journal.tx_index, i as u32, "tx_index mismatch for tx {i}");
        assert_ne!(journal.input.state_root, [0u8; 32], "expected non-zero state_root for tx {i}");
        assert_eq!(journal.output.ops_hash, expected_ops_hash, "ops_hash mismatch for tx {i}");
    }

    scheduler.shutdown();
}
