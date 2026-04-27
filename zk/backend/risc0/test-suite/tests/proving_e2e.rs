use std::num::NonZeroUsize;

use kaspa_consensus_core::hashing::tx::id as kaspa_tx_id;
use kaspa_hashes::Hash;
use kaspa_rpc_core::api::rpc::RpcApi;
use tempfile::TempDir;
use vprogs_core_smt::{EMPTY_HASH, Tree as _};
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId, SchedulerTransaction};
use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};
use vprogs_node_test_utils::L1Node;
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler};
use vprogs_state_version::StateVersion;
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_abi::{batch_processor::StateTransition, transaction_processor::JournalEntries};
use vprogs_zk_backend_risc0_api::Backend;
use vprogs_zk_backend_risc0_test_suite::{
    batch_processor_elf, compute_section_lane_tip, transaction_processor_elf,
};
use vprogs_zk_batch_prover::{Backend as _, BatchProverConfig};
use vprogs_zk_vm::{ProvingPipeline, Vm};

/// Builds a `ChainBlockMetadata` from a real simnet block. Required because the bundling
/// prover calls `get_seq_commit_lane_proof(block_hash, lane_key)` which only resolves for
/// blocks that actually exist on the simnet.
async fn metadata_for_block(l1: &L1Node, block_hash: Hash) -> ChainBlockMetadata {
    let block = l1.grpc_client().get_block(block_hash, false).await.expect("get_block");
    let h = block.header;
    ChainBlockMetadata {
        hash: h.hash,
        blue_score: h.blue_score,
        daa_score: h.daa_score,
        timestamp: h.timestamp,
        seq_commit: h.accepted_id_merkle_root,
        ..Default::default()
    }
}

/// Proves two transactions that each increment a u32 counter on distinct resources.
///
/// Verifies the full pipeline: execution -> transaction proving -> bundle proving (K=1) -> state
/// root transition. Uses a real simnet block hash so the prover's `get_seq_commit_lane_proof`
/// RPC call resolves; with `lane_key = 0` and no lane activity, the response carries a
/// non-inclusion proof and the pre-prove sanity check is skipped.
#[tokio::test(flavor = "multi_thread")]
async fn batch_proof_two_transactions() {
    let transaction_elf = transaction_processor_elf();
    let batch_elf = batch_processor_elf();

    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());

    let backend = Backend::new(&transaction_elf, &batch_elf);

    let l1 = L1Node::new(None).await;
    // Mine a couple of blocks so we have real block hashes to anchor metadata against.
    let block_hashes = l1.mine_blocks(2).await;

    let config =
        BatchProverConfig { bundle_size: NonZeroUsize::new(1).unwrap(), lane_key: Hash::default() };

    let proving =
        ProvingPipeline::batch(backend.clone(), storage.clone(), l1.grpc_client().clone(), config);
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

    let metadata_1 = metadata_for_block(&l1, block_hashes[0]).await;

    let batch = scheduler.schedule(
        metadata_1,
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
    // K=1 again, so this batch becomes a separate bundle of one and chains state from batch 1.

    let mut tx3 = L1Transaction::default();
    tx3.version = 1;
    tx3.payload = vec![7, 8, 9];
    let mut tx4 = L1Transaction::default();
    tx4.version = 1;
    tx4.payload = vec![10, 11, 12];

    let expected_tx_ids_2 = [kaspa_tx_id(&tx3).as_bytes(), kaspa_tx_id(&tx4).as_bytes()];

    let metadata_2 = metadata_for_block(&l1, block_hashes[1]).await;

    let batch_2 = scheduler.schedule(
        metadata_2,
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
    l1.shutdown().await;
}

/// Bundles K=2 batches into a single proof. Two scheduled batches share one outer receipt
/// covering both state transitions; the bundle's `prev_state` is the pre-batch-1 root and
/// its `new_state` is the post-batch-2 root.
#[tokio::test(flavor = "multi_thread")]
async fn batch_proof_bundle_of_two() {
    let transaction_elf = transaction_processor_elf();
    let batch_elf = batch_processor_elf();

    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());

    let backend = Backend::new(&transaction_elf, &batch_elf);

    let l1 = L1Node::new(None).await;
    let block_hashes = l1.mine_blocks(2).await;

    let config =
        BatchProverConfig { bundle_size: NonZeroUsize::new(2).unwrap(), lane_key: Hash::default() };

    let proving =
        ProvingPipeline::batch(backend.clone(), storage.clone(), l1.grpc_client().clone(), config);
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

    let lane_key = [0u8; 32];
    let metadata_1 = metadata_for_block(&l1, block_hashes[0]).await;
    let batch_1 = scheduler.schedule(
        metadata_1.clone(),
        vec![
            SchedulerTransaction::new(
                0,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                tx1.clone(),
            ),
            SchedulerTransaction::new(
                1,
                vec![AccessMetadata::write(ResourceId::for_test(2))],
                tx2.clone(),
            ),
        ],
    );

    let mut tx3 = L1Transaction::default();
    tx3.version = 1;
    tx3.payload = vec![7, 8, 9];
    let mut tx4 = L1Transaction::default();
    tx4.version = 1;
    tx4.payload = vec![10, 11, 12];

    // Production runs through the bridge, which chains `lane_tip` block to block so
    // section[i+1].prev_lane_tip == section[i].new_lane_tip. Tests bypass the bridge and
    // construct metadata fresh from each block, so we derive the chain ourselves here using
    // the same `lane_tip_next` the guest uses internally.
    let mut metadata_2 = metadata_for_block(&l1, block_hashes[1]).await;
    metadata_2.prev_lane_tip =
        compute_section_lane_tip(&metadata_1, &[(0, &tx1), (1, &tx2)], &lane_key);
    let batch_2 = scheduler.schedule(
        metadata_2,
        vec![
            SchedulerTransaction::new(0, vec![AccessMetadata::write(ResourceId::for_test(1))], tx3),
            SchedulerTransaction::new(1, vec![AccessMetadata::write(ResourceId::for_test(2))], tx4),
        ],
    );

    // Both batches end up in a single bundle (K=2). The same outer receipt is published to
    // both, with `prev_state` = pre-batch-1 root and `new_state` = post-batch-2 root.
    batch_1.wait_committed_blocking();
    batch_2.wait_committed_blocking();
    batch_1.wait_artifact_published_blocking();
    batch_2.wait_artifact_published_blocking();

    let receipt_1 = batch_1.artifact();
    let receipt_2 = batch_2.artifact();
    let journal_1 = Backend::journal_bytes(&receipt_1);
    let journal_2 = Backend::journal_bytes(&receipt_2);

    // Same bundle receipt published to both batches → identical journals.
    assert_eq!(journal_1, journal_2, "bundle publishes the same receipt to every batch");

    let state = StateTransition::decode(&journal_1).expect("bundle journal should decode");
    assert_eq!(state.prev_state, EMPTY_HASH, "bundle prev_state is the bundle's start");
    assert_eq!(state.new_state, storage.root(2), "bundle new_state is post-batch-2 root");

    // Per-resource state: counter incremented twice (1 → 2) by the two batches in the bundle.
    let r1_v2 = StateVersion::get(&storage, 2, &ResourceId::for_test(1))
        .expect("resource 1 should have v2 data");
    let r2_v2 = StateVersion::get(&storage, 2, &ResourceId::for_test(2))
        .expect("resource 2 should have v2 data");
    assert_eq!(u32::from_le_bytes(r1_v2.as_slice().try_into().unwrap()), 2);
    assert_eq!(u32::from_le_bytes(r2_v2.as_slice().try_into().unwrap()), 2);

    scheduler.shutdown();
    l1.shutdown().await;
}
