use tempfile::TempDir;
use vprogs_core_atomics::AsyncQueue;
use vprogs_core_smt::{EMPTY_HASH, Tree as _};
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId, SchedulerTransaction};
use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler};
use vprogs_state_version::StateVersion;
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_abi::batch_processor::StateTransition;
use vprogs_zk_backend_risc0_api::Backend;
use vprogs_zk_batch_prover::Backend as _;
use vprogs_zk_transaction_prover::Backend as _;
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

/// Proves two transactions that each increment a u32 counter on distinct resources.
///
/// Verifies the full pipeline: execution -> transaction proving -> batch proving -> state root
/// transition. The guest increments each resource's counter from 0 to 1, so prev_root should
/// differ from new_root (state changed).
#[tokio::test(flavor = "multi_thread")]
async fn batch_proof_two_transactions() {
    let transaction_elf = transaction_processor_elf();
    let batch_elf = batch_processor_elf();

    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());

    let backend = Backend::new(&transaction_elf, &batch_elf);

    // Create the VM with batch proving enabled.
    let batch_proof_rx = AsyncQueue::new();
    let proving = ProvingPipeline::batch(backend.clone(), storage.clone(), batch_proof_rx.clone());
    let vm = Vm::new(backend.clone(), proving);

    let mut scheduler = Scheduler::new(
        ExecutionConfig::default().with_processor(vm),
        StorageConfig::default().with_store(storage.clone()),
    );

    // Create two transactions touching distinct resources.
    let mut tx1 = L1Transaction::default();
    tx1.payload = vec![1, 2, 3];
    let mut tx2 = L1Transaction::default();
    tx2.payload = vec![4, 5, 6];

    let block_metadata = ChainBlockMetadata::default();

    // Schedule the batch (executes both transactions and starts proving).
    let batch = scheduler.schedule(
        block_metadata,
        vec![
            SchedulerTransaction::new(tx1, vec![AccessMetadata::write(ResourceId::for_test(1))]),
            SchedulerTransaction::new(tx2, vec![AccessMetadata::write(ResourceId::for_test(2))]),
        ],
    );

    batch.wait_committed_blocking();

    // Wait for the batch proof receipt.
    let receipt = batch_proof_rx.wait_and_pop().await;
    let journal = Backend::journal_bytes(&receipt);

    // Decode the state transition from the receipt journal.
    match StateTransition::decode(&journal).expect("journal should decode") {
        StateTransition::Success { image_id: journal_image_id, prev_root, new_root } => {
            assert_eq!(journal_image_id, backend.image_id());
            assert_ne!(prev_root, new_root, "state should change after counter increment");
            assert_eq!(*prev_root, EMPTY_HASH, "prev_root should be empty (no prior state)");
            assert_eq!(*new_root, storage.root(1), "new_root should match store's version 1");
        }
        StateTransition::Error(e) => panic!("expected success, got error: {e}"),
    }

    // Verify committed counter values - both resources should be 1 (incremented from 0).
    let r1_data = StateVersion::get(&storage, 1, &ResourceId::for_test(1))
        .expect("resource 1 should have committed data");
    let r2_data = StateVersion::get(&storage, 1, &ResourceId::for_test(2))
        .expect("resource 2 should have committed data");
    assert_eq!(u32::from_le_bytes(r1_data.as_slice().try_into().unwrap()), 1);
    assert_eq!(u32::from_le_bytes(r2_data.as_slice().try_into().unwrap()), 1);

    // Verify SMT proof leaves contain correct value hashes.
    let expected_hash = *blake3::hash(&1u32.to_le_bytes()).as_bytes();
    for resource_id in [ResourceId::for_test(1), ResourceId::for_test(2)] {
        let (proof_bytes, _) = storage.prove(&[*resource_id.as_bytes()], 1).unwrap();
        let smt_proof = vprogs_core_smt::proving::Proof::decode(&proof_bytes).unwrap();
        assert_eq!(*smt_proof.leaves[0].value_hash, expected_hash);
    }

    // --- Second batch: increment counters from 1 to 2 ---

    let block_metadata_2 = ChainBlockMetadata::default();

    let mut tx3 = L1Transaction::default();
    tx3.payload = vec![7, 8, 9];
    let mut tx4 = L1Transaction::default();
    tx4.payload = vec![10, 11, 12];

    let batch_2 = scheduler.schedule(
        block_metadata_2,
        vec![
            SchedulerTransaction::new(tx3, vec![AccessMetadata::write(ResourceId::for_test(1))]),
            SchedulerTransaction::new(tx4, vec![AccessMetadata::write(ResourceId::for_test(2))]),
        ],
    );

    batch_2.wait_committed_blocking();

    let receipt_2 = batch_proof_rx.wait_and_pop().await;
    let journal_2 = Backend::journal_bytes(&receipt_2);

    // Chain continuity: batch 2's prev_root should equal batch 1's new_root.
    match StateTransition::decode(&journal_2).expect("journal should decode") {
        StateTransition::Success { prev_root, new_root, .. } => {
            assert_eq!(*prev_root, storage.root(1), "batch 2 prev_root should chain from batch 1");
            assert_ne!(prev_root, new_root, "state should change again");
        }
        StateTransition::Error(e) => panic!("expected success, got error: {e}"),
    }

    // Verify counter values are now 2.
    let r1_v2 = StateVersion::get(&storage, 2, &ResourceId::for_test(1))
        .expect("resource 1 should have v2 data");
    let r2_v2 = StateVersion::get(&storage, 2, &ResourceId::for_test(2))
        .expect("resource 2 should have v2 data");
    assert_eq!(u32::from_le_bytes(r1_v2.as_slice().try_into().unwrap()), 2);
    assert_eq!(u32::from_le_bytes(r2_v2.as_slice().try_into().unwrap()), 2);

    // Verify SMT leaves at version 2.
    let expected_hash_v2 = *blake3::hash(&2u32.to_le_bytes()).as_bytes();
    for resource_id in [ResourceId::for_test(1), ResourceId::for_test(2)] {
        let (proof_bytes, _) = storage.prove(&[*resource_id.as_bytes()], 2).unwrap();
        let smt_proof = vprogs_core_smt::proving::Proof::decode(&proof_bytes).unwrap();
        assert_eq!(*smt_proof.leaves[0].value_hash, expected_hash_v2);
    }

    scheduler.shutdown();
}
