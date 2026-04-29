use std::num::NonZeroUsize;

use kaspa_consensus_core::tx::{CovenantBinding, TransactionOutpoint};
use kaspa_hashes::Hash;
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_txscript::{standard::pay_to_script_hash_script, zk_precompiles::tags::ZkTag};
use tempfile::TempDir;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId, SchedulerTransaction};
use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};
use vprogs_node_test_utils::L1Node;
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler};
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_backend_risc0_api::Backend;
use vprogs_zk_backend_risc0_test_suite::{
    batch_processor_elf, compute_section_lane_tip, transaction_processor_elf,
};
use vprogs_zk_batch_prover::{Backend as _, BatchProverConfig};
use vprogs_zk_covenant::{
    Settlement, SettlementInput, StateTransition, SuccinctWitness, build_redeem_script,
    redeem_script_len,
};
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

/// Asserts that `settlement.transaction` has the expected single covenant input and single
/// continuation output, and that the redeem prefix the script binds is the one we built
/// from the journal's `prev_state` / `prev_lane_tip`.
fn assert_settlement_structure(
    settlement: &Settlement,
    parsed: &StateTransition,
    program_id: &[u8; 32],
    tx_image_id: &[u8; 32],
    covenant_id_hash: Hash,
) {
    assert_eq!(settlement.transaction.inputs.len(), 1);
    assert_eq!(settlement.transaction.outputs.len(), 1);
    assert_eq!(
        settlement.transaction.outputs[0].script_public_key,
        pay_to_script_hash_script(&settlement.next_redeem),
    );
    assert_eq!(
        settlement.transaction.outputs[0].covenant,
        Some(CovenantBinding::new(0, covenant_id_hash)),
    );

    let expected_len =
        redeem_script_len(parsed.prev_state, program_id, tx_image_id, ZkTag::R0Succinct);
    let expected_prev_redeem = build_redeem_script(
        parsed.prev_state,
        parsed.prev_lane_tip,
        expected_len,
        program_id,
        tx_image_id,
        ZkTag::R0Succinct,
    );
    assert_eq!(settlement.prev_redeem, expected_prev_redeem);
    assert_eq!(settlement.prev_redeem.len(), settlement.next_redeem.len());
}

/// K=1 - single-batch bundle. Verifies that the bundling pipeline produces a 224-byte
/// settlement journal that can be wrapped in a host-side `Settlement::build` call.
#[tokio::test(flavor = "multi_thread")]
async fn batch_proof_is_directly_settleable_single_batch() {
    let transaction_elf = transaction_processor_elf();
    let batch_elf = batch_processor_elf();

    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());

    let backend = Backend::new(&transaction_elf, &batch_elf);

    let l1 = L1Node::new(None).await;
    let block_hashes = l1.mine_blocks(1).await;

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

    let metadata = metadata_for_block(&l1, block_hashes[0]).await;
    let batch = scheduler.schedule(
        metadata,
        vec![
            SchedulerTransaction::new(0, vec![AccessMetadata::write(ResourceId::for_test(1))], tx1),
            SchedulerTransaction::new(1, vec![AccessMetadata::write(ResourceId::for_test(2))], tx2),
        ],
    );

    batch.wait_committed_blocking();
    batch.wait_artifact_published_blocking();

    let batch_receipt = (*batch.artifact()).clone();
    let journal_bytes = Backend::journal_bytes(&batch_receipt);

    assert_eq!(
        journal_bytes.len(),
        vprogs_zk_covenant::JOURNAL_SIZE,
        "batch journal must be exactly {} bytes",
        vprogs_zk_covenant::JOURNAL_SIZE,
    );

    let parsed = StateTransition::decode(&journal_bytes)
        .expect("batch journal must decode as a state transition");
    // covenant_id is zero in the non-settling test path (see batch-prover/src/worker.rs).
    assert_eq!(parsed.covenant_id, &[0u8; 32]);
    assert_eq!(parsed.prev_lane_tip, &[0u8; 32], "first section's prev_lane_tip is bundle's start");

    let program_id = *backend.batch_image_id();
    let tx_image_id = *backend.transaction_image_id();
    assert_eq!(
        parsed.tx_image_id, &tx_image_id,
        "guest must echo the host-supplied tx image id into the journal",
    );
    let covenant_id_hash = Hash::from_bytes(*parsed.covenant_id);
    let settlement = Settlement::build(&SettlementInput {
        covenant_id: covenant_id_hash,
        program_id: &program_id,
        tx_image_id: &tx_image_id,
        prev_state: parsed.prev_state,
        prev_lane_tip: parsed.prev_lane_tip,
        new_state: parsed.new_state,
        new_lane_tip: parsed.new_lane_tip,
        block_prove_to: block_hashes[0],
        prev_outpoint: TransactionOutpoint::new(Hash::from_bytes([0xCD; 32]), 0),
        value: 100_000_000,
        witness: SuccinctWitness {
            seal: &[0u8; 8],
            claim: &[0u8; 32],
            control_id: &[0u8; 32],
            hashfn: 0,
            control_index: 0,
            control_digests: &[],
        },
    });
    assert_settlement_structure(&settlement, &parsed, &program_id, &tx_image_id, covenant_id_hash);

    scheduler.shutdown();
    l1.shutdown().await;
}

/// K=2 - bundles two batches into one bundle proof + one settlement. Verifies that the
/// bundle's outer journal carries the correct endpoint state (`prev_state` = pre-batch-1,
/// `new_state` = post-batch-2) and that the same receipt is published to both batches.
#[tokio::test(flavor = "multi_thread")]
async fn batch_proof_bundles_two_batches() {
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

    // Batch 1: counters 0 → 1.
    let mut tx1 = L1Transaction::default();
    tx1.version = 1;
    tx1.payload = vec![1, 2, 3];
    let mut tx2 = L1Transaction::default();
    tx2.version = 1;
    tx2.payload = vec![4, 5, 6];

    let lane_key = [0u8; 32];
    let metadata_1 = metadata_for_block(&l1, block_hashes[0]).await;
    let batch_1 = scheduler.schedule(
        metadata_1,
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

    // Batch 2: counters 1 → 2.
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

    batch_1.wait_committed_blocking();
    batch_2.wait_committed_blocking();
    batch_1.wait_artifact_published_blocking();
    batch_2.wait_artifact_published_blocking();

    let r1 = (*batch_1.artifact()).clone();
    let r2 = (*batch_2.artifact()).clone();
    let j1 = Backend::journal_bytes(&r1);
    let j2 = Backend::journal_bytes(&r2);
    assert_eq!(j1, j2, "bundle publishes the same receipt to every batch");
    assert_eq!(j1.len(), vprogs_zk_covenant::JOURNAL_SIZE);

    let parsed = StateTransition::decode(&j1).expect("bundle journal should decode");
    assert_eq!(parsed.covenant_id, &[0u8; 32]);
    assert_eq!(parsed.prev_lane_tip, &[0u8; 32], "bundle prev_lane_tip is bundle's start");

    let program_id = *backend.batch_image_id();
    let tx_image_id = *backend.transaction_image_id();
    let covenant_id_hash = Hash::from_bytes(*parsed.covenant_id);
    let settlement = Settlement::build(&SettlementInput {
        covenant_id: covenant_id_hash,
        program_id: &program_id,
        tx_image_id: &tx_image_id,
        prev_state: parsed.prev_state,
        prev_lane_tip: parsed.prev_lane_tip,
        new_state: parsed.new_state,
        new_lane_tip: parsed.new_lane_tip,
        // Anchor to the bundle's *final* block - the covenant only verifies one seq_commit.
        block_prove_to: block_hashes[1],
        prev_outpoint: TransactionOutpoint::new(Hash::from_bytes([0xCD; 32]), 0),
        value: 100_000_000,
        witness: SuccinctWitness {
            seal: &[0u8; 8],
            claim: &[0u8; 32],
            control_id: &[0u8; 32],
            hashfn: 0,
            control_index: 0,
            control_digests: &[],
        },
    });
    assert_settlement_structure(&settlement, &parsed, &program_id, &tx_image_id, covenant_id_hash);

    scheduler.shutdown();
    l1.shutdown().await;
}
