use kaspa_consensus_core::tx::{CovenantBinding, TransactionOutpoint};
use kaspa_hashes::Hash;
use kaspa_txscript::{standard::pay_to_script_hash_script, zk_precompiles::tags::ZkTag};
use tempfile::TempDir;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId, SchedulerTransaction};
use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler};
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_backend_risc0_api::Backend;
use vprogs_zk_backend_risc0_test_suite::{batch_processor_elf, transaction_processor_elf};
use vprogs_zk_batch_prover::Backend as _;
use vprogs_zk_covenant::{
    Settlement, SettlementInput, StateTransition, SuccinctWitness, build_redeem_script,
    redeem_script_len,
};
use vprogs_zk_vm::{ProvingPipeline, Vm};

/// Runs a batch proof through the full pipeline and asserts the batch receipt's journal is
/// exactly the 224-byte settlement preimage the covenant script expects. The batch processor
/// emits the settlement journal directly — no wrapping guest.
#[tokio::test(flavor = "multi_thread")]
async fn batch_proof_is_directly_settleable() {
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

    // Non-zero seq_commit / prev_seq_commit so the settlement journal carries meaningful data.
    let block_metadata = ChainBlockMetadata {
        hash: Hash::from_bytes([0x77; 32]),
        seq_commit: Hash::from_bytes([0x88; 32]),
        prev_seq_commit: Hash::from_bytes([0x99; 32]),
        ..Default::default()
    };

    let batch = scheduler.schedule(
        block_metadata,
        vec![
            SchedulerTransaction::new(0, vec![AccessMetadata::write(ResourceId::for_test(1))], tx1),
            SchedulerTransaction::new(1, vec![AccessMetadata::write(ResourceId::for_test(2))], tx2),
        ],
    );

    batch.wait_committed_blocking();
    batch.wait_artifact_published_blocking();

    let batch_receipt = (*batch.artifact()).clone();
    let journal_bytes = Backend::journal_bytes(&batch_receipt);

    // The batch journal IS the settlement preimage now - no wrapping guest.
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
    // `prev_lane_tip` defaults to the empty-hash zero value when ChainBlockMetadata doesn't
    // carry a previous tip (non-settling run).
    assert_eq!(parsed.prev_lane_tip, &[0u8; 32], "prev_lane_tip echoes metadata's empty default");
    // This test doesn't populate lane_smt_proof/miner_payload_leaves in ChainBlockMetadata,
    // so the guest's kip21 derivation is skipped and new_seq_commit is emitted as zero. The
    // covenant would reject this receipt on-chain — which is correct behavior for a
    // non-settling run.
    assert_eq!(
        parsed.new_seq_commit, [0u8; 32],
        "new_seq_commit is zero when lane ingredients missing"
    );

    // Build the host-side settlement transaction against the real batch + tx image ids.
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
        prev_state: &parsed.prev_state,
        prev_lane_tip: parsed.prev_lane_tip,
        new_state: &parsed.new_state,
        new_lane_tip: &parsed.new_lane_tip,
        block_prove_to: Hash::from_bytes([0xAB; 32]),
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
        redeem_script_len(&parsed.prev_state, &program_id, &tx_image_id, ZkTag::R0Succinct);
    let expected_prev_redeem = build_redeem_script(
        &parsed.prev_state,
        parsed.prev_lane_tip,
        expected_len,
        &program_id,
        &tx_image_id,
        ZkTag::R0Succinct,
    );
    assert_eq!(settlement.prev_redeem, expected_prev_redeem);
    assert_eq!(settlement.prev_redeem.len(), settlement.next_redeem.len());

    scheduler.shutdown();
}
