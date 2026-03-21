use risc0_binfmt::ProgramBinary;
use tempfile::TempDir;
use tokio::sync::mpsc;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId, SchedulerTransaction};
use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler};
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_abi::batch_processor::StateTransition;
use vprogs_zk_backend_risc0_api::Backend;
use vprogs_zk_prover::BatchProver;
use vprogs_zk_vm::Vm;

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

/// Proves two transactions individually, then stitches them into a single batch proof.
///
/// Verifies the full pipeline: execution → transaction proving → batch proving → state root
/// transition.
#[tokio::test(flavor = "multi_thread")]
async fn batch_proof_two_transactions() {
    let transaction_elf = transaction_processor_elf();
    let batch_elf = batch_processor_elf();

    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
    let store_for_prover = storage.clone();

    let backend = Backend::new(&transaction_elf, &batch_elf);
    let image_id: [u8; 32] =
        ProgramBinary::new(&transaction_elf, risc0_zkos_v1compat::V1COMPAT_ELF)
            .compute_image_id()
            .expect("failed to compute image ID")
            .as_bytes()
            .try_into()
            .unwrap();

    // Set up the proof channel between VM and prover.
    let (proof_tx, proof_rx) = mpsc::unbounded_channel();
    let vm = Vm::with_proof_channel(backend.clone(), proof_tx);

    let mut scheduler = Scheduler::new(
        ExecutionConfig::default().with_processor(vm),
        StorageConfig::default().with_store(storage),
    );

    // Create two transactions touching distinct resources.
    let mut tx1 = L1Transaction::default();
    tx1.payload = vec![1, 2, 3];
    let mut tx2 = L1Transaction::default();
    tx2.payload = vec![4, 5, 6];

    let block_metadata = ChainBlockMetadata::default();
    let block_hash = block_metadata.block_hash().as_bytes();

    // Set up the batch prover. Version 1 = first scheduled batch in the store.
    let mut batch_prover = BatchProver::new(backend, proof_rx, image_id, store_for_prover);
    batch_prover.register_batch(block_hash, 2, 1);

    // Start the proving loop.
    let mut batch_proof_rx = batch_prover.run().await;

    // Schedule the batch (this executes both transactions and sends proof requests).
    let batch = scheduler.schedule(
        block_metadata,
        vec![
            SchedulerTransaction::new(tx1, vec![AccessMetadata::write(ResourceId::for_test(1))]),
            SchedulerTransaction::new(tx2, vec![AccessMetadata::write(ResourceId::for_test(2))]),
        ],
    );

    batch.wait_committed_blocking();

    // Wait for the batch proof.
    let proof = batch_proof_rx.recv().await.expect("expected a batch proof");

    // Verify basic proof structure.
    assert_eq!(proof.block_hash, block_hash);
    assert_eq!(proof.batch_index, 0);
    assert!(!proof.receipt_journal.is_empty(), "receipt journal should not be empty");

    // The journal encodes (image_id, prev_root, new_root).
    // The guest is a no-op (doesn't modify resources), so prev_root == new_root.
    // Once the guest implements actual execution, this should change.
    assert_eq!(proof.prev_root, proof.new_root);

    // Verify journal contents match proof fields via the decoder.
    match StateTransition::decode(&proof.receipt_journal).expect("journal should decode") {
        StateTransition::Success { image_id: journal_image_id, prev_root, new_root } => {
            assert_eq!(*journal_image_id, image_id);
            assert_eq!(*prev_root, proof.prev_root);
            assert_eq!(*new_root, proof.new_root);
        }
        StateTransition::Error(e) => panic!("expected success, got error: {e}"),
    }

    scheduler.shutdown();
}
