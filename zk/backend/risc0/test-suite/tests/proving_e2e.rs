use kaspa_consensus_core::hashing::tx::id as kaspa_tx_id;
use tempfile::TempDir;
use vprogs_core_smt::{EMPTY_HASH, Tree as _};
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId, SchedulerTransaction};
use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler};
use vprogs_state_version::StateVersion;
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_abi::{batch_processor::StateTransition, transaction_processor::JournalEntries};
use vprogs_zk_backend_risc0_api::Backend;
use vprogs_zk_backend_risc0_test_suite::{batch_processor_elf, transaction_processor_elf};
use vprogs_zk_batch_prover::Backend as _;
use vprogs_zk_transaction_prover::Backend as _;
use vprogs_zk_vm::{ProvingPipeline, Vm};

/// Proves two transactions that each increment a u32 counter on distinct resources.
///
/// Verifies the full pipeline: execution -> transaction proving -> batch proving -> state root
/// transition. The guest increments each resource's counter from 0 to 1, so prev_state should
/// differ from new_state (state changed).
#[tokio::test(flavor = "multi_thread")]
async fn batch_proof_two_transactions() {
    let transaction_elf = transaction_processor_elf();
    let batch_elf = batch_processor_elf();

    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());

    let backend = Backend::new(&transaction_elf, &batch_elf);

    let proving = ProvingPipeline::batch(backend.clone(), storage.clone());
    let vm = Vm::new(backend.clone(), proving);

    let mut scheduler = Scheduler::new(
        ExecutionConfig::default().with_processor(vm),
        StorageConfig::default().with_store(storage.clone()),
    );

    let mut tx1 = L1Transaction::default();
    tx1.version = 1;
    tx1.payload = vec![1, 2, 3];
    let mut tx2 = L1Transaction::default();
    tx2.version = 1;
    tx2.payload = vec![4, 5, 6];

    let expected_tx_ids = [kaspa_tx_id(&tx1).as_bytes(), kaspa_tx_id(&tx2).as_bytes()];

    let batch = scheduler.schedule(
        ChainBlockMetadata::default(),
        vec![
            SchedulerTransaction::new(0, vec![AccessMetadata::write(ResourceId::for_test(1))], tx1),
            SchedulerTransaction::new(1, vec![AccessMetadata::write(ResourceId::for_test(2))], tx2),
        ],
    );

    batch.wait_committed_blocking();
    batch.wait_artifact_published_blocking();

    for (artifact, expected) in batch.tx_artifacts().zip(expected_tx_ids.iter()) {
        let journal = Backend::journal_bytes(&artifact);
        let parsed = JournalEntries::decode(&journal).expect("valid tx journal");
        assert_eq!(
            parsed.input_commitment.tx_id, expected,
            "committed tx_id must match kaspa's native id"
        );
    }

    let receipt = batch.artifact();
    let journal = Backend::journal_bytes(&receipt);

    let state = StateTransition::decode(&journal).expect("journal should decode");
    assert_ne!(state.prev_state, state.new_state, "state should change after counter increment");
    assert_eq!(state.prev_state, EMPTY_HASH, "prev_state should be empty (no prior state)");
    assert_eq!(state.new_state, storage.root(1), "new_state should match store's version 1");

    let r1_data = StateVersion::get(&storage, 1, &ResourceId::for_test(1))
        .expect("resource 1 should have committed data");
    let r2_data = StateVersion::get(&storage, 1, &ResourceId::for_test(2))
        .expect("resource 2 should have committed data");
    assert_eq!(u32::from_le_bytes(r1_data.as_slice().try_into().unwrap()), 1);
    assert_eq!(u32::from_le_bytes(r2_data.as_slice().try_into().unwrap()), 1);

    let expected_hash = *blake3::hash(&1u32.to_le_bytes()).as_bytes();
    for resource_id in [ResourceId::for_test(1), ResourceId::for_test(2)] {
        let (proof_bytes, _) = storage.prove(&[resource_id], 1).unwrap();
        let smt_proof = vprogs_core_smt::proving::Proof::decode(&proof_bytes).unwrap();
        assert_eq!(*smt_proof.leaves[0].value_hash, expected_hash);
    }

    // --- Second batch: increment counters from 1 to 2 ---

    let mut tx3 = L1Transaction::default();
    tx3.version = 1;
    tx3.payload = vec![7, 8, 9];
    let mut tx4 = L1Transaction::default();
    tx4.version = 1;
    tx4.payload = vec![10, 11, 12];

    let expected_tx_ids_2 = [kaspa_tx_id(&tx3).as_bytes(), kaspa_tx_id(&tx4).as_bytes()];

    let batch_2 = scheduler.schedule(
        ChainBlockMetadata::default(),
        vec![
            SchedulerTransaction::new(0, vec![AccessMetadata::write(ResourceId::for_test(1))], tx3),
            SchedulerTransaction::new(1, vec![AccessMetadata::write(ResourceId::for_test(2))], tx4),
        ],
    );

    batch_2.wait_committed_blocking();
    batch_2.wait_artifact_published_blocking();

    for (artifact, expected) in batch_2.tx_artifacts().zip(expected_tx_ids_2.iter()) {
        let journal = Backend::journal_bytes(&artifact);
        let parsed = JournalEntries::decode(&journal).expect("valid tx journal");
        assert_eq!(
            parsed.input_commitment.tx_id, expected,
            "committed tx_id must match kaspa's native id"
        );
    }

    let receipt_2 = batch_2.artifact();
    let journal_2 = Backend::journal_bytes(&receipt_2);

    let state_2 = StateTransition::decode(&journal_2).expect("journal should decode");
    assert_eq!(state_2.prev_state, storage.root(1), "batch 2 prev_state should chain from batch 1");
    assert_ne!(state_2.prev_state, state_2.new_state, "state should change again");

    let r1_v2 = StateVersion::get(&storage, 2, &ResourceId::for_test(1))
        .expect("resource 1 should have v2 data");
    let r2_v2 = StateVersion::get(&storage, 2, &ResourceId::for_test(2))
        .expect("resource 2 should have v2 data");
    assert_eq!(u32::from_le_bytes(r1_v2.as_slice().try_into().unwrap()), 2);
    assert_eq!(u32::from_le_bytes(r2_v2.as_slice().try_into().unwrap()), 2);

    let expected_hash_v2 = *blake3::hash(&2u32.to_le_bytes()).as_bytes();
    for resource_id in [ResourceId::for_test(1), ResourceId::for_test(2)] {
        let (proof_bytes, _) = storage.prove(&[resource_id], 2).unwrap();
        let smt_proof = vprogs_core_smt::proving::Proof::decode(&proof_bytes).unwrap();
        assert_eq!(*smt_proof.leaves[0].value_hash, expected_hash_v2);
    }

    scheduler.shutdown();
}
