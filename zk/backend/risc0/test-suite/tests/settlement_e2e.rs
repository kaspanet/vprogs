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
use vprogs_zk_abi::batch_processor::StateTransition;
use vprogs_zk_backend_risc0_api::Backend;
use vprogs_zk_backend_risc0_test_suite::{
    batch_processor_elf, settlement_processor_elf, transaction_processor_elf,
};
use vprogs_zk_batch_prover::Backend as _;
use vprogs_zk_covenant::{
    Settlement, SettlementInput, SettlementJournal, SuccinctWitness, build_redeem_script,
    redeem_script_len,
};
use vprogs_zk_vm::{ProvingPipeline, Vm};

/// Runs a batch proof through the full pipeline, then wraps it with the settlement guest and
/// asserts that the settlement receipt's journal is exactly the 160-byte layout the covenant
/// script expects.
#[tokio::test(flavor = "multi_thread")]
async fn settlement_proof_over_single_batch() {
    let transaction_elf = transaction_processor_elf();
    let batch_elf = batch_processor_elf();
    let settlement_elf = settlement_processor_elf();

    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());

    let backend = Backend::new(&transaction_elf, &batch_elf, Some(&settlement_elf));

    let lane_key = kaspa_seq_commit::hashing::lane_key(&[0x42; 20]).as_bytes();
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

    let block_metadata = ChainBlockMetadata {
        hash: Hash::from_bytes([0x77; 32]),
        seq_commit: Hash::from_bytes([0x88; 32]),
        prev_seq_commit: Hash::from_bytes([0x99; 32]),
        lane_key,
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
    let batch_journal = Backend::journal_bytes(&batch_receipt);

    let (expected_prev_state, expected_new_state, expected_prev_seq, expected_new_seq) =
        match StateTransition::decode(&batch_journal).expect("batch journal decodes") {
            StateTransition::Success {
                prev_root, new_root, seq_commit, prev_seq_commit, ..
            } => (*prev_root, *new_root, *prev_seq_commit, *seq_commit),
            StateTransition::Error(e) => panic!("expected batch success, got error: {e}"),
        };

    let covenant_id = [0xC0u8; 32];
    let settlement_receipt = backend.prove_settlement(&covenant_id, batch_receipt).await;

    settlement_receipt
        .verify(*backend.settlement_image_id().expect("settlement image id present"))
        .expect("settlement receipt verifies against its own image id");

    let settlement_journal = settlement_receipt.journal.bytes;
    assert_eq!(
        settlement_journal.len(),
        vprogs_zk_covenant::JOURNAL_SIZE,
        "settlement journal must be exactly {} bytes",
        vprogs_zk_covenant::JOURNAL_SIZE,
    );

    let parsed = SettlementJournal::decode(&settlement_journal)
        .expect("settlement journal must decode at the canonical layout");
    assert_eq!(parsed.prev_state, &expected_prev_state);
    assert_eq!(parsed.prev_seq, &expected_prev_seq);
    assert_eq!(parsed.new_state, &expected_new_state);
    assert_eq!(parsed.new_seq, &expected_new_seq);
    assert_eq!(parsed.covenant_id, &covenant_id);

    // Build the host-side settlement transaction. We don't submit it here (that's the simnet
    // integration test) - just verify the structure matches what a covenant-enabled L1 expects:
    // single output SPK = P2SH of the new redeem script, covenant binding preserved.
    let program_id = *backend.settlement_image_id().expect("settlement image id present");
    let covenant_id_hash = Hash::from_bytes(covenant_id);
    let settlement = Settlement::build(&SettlementInput {
        covenant_id: covenant_id_hash,
        program_id: &program_id,
        prev_state: &expected_prev_state,
        prev_seq: &expected_prev_seq,
        new_state: &expected_new_state,
        new_seq: &expected_new_seq,
        block_prove_to: Hash::from_bytes([0xAB; 32]),
        prev_outpoint: TransactionOutpoint::new(Hash::from_bytes([0xCD; 32]), 0),
        value: 100_000_000,
        witness: SuccinctWitness {
            seal: &[0u8; 8],
            claim: &[0u8; 32],
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
        "continuation SPK must be P2SH of the next redeem",
    );
    assert_eq!(
        settlement.transaction.outputs[0].covenant,
        Some(CovenantBinding::new(0, covenant_id_hash)),
        "continuation output must preserve the covenant id",
    );

    // Sanity-check the redeem-script-length self-referential invariant holds for both ends.
    let expected_len = redeem_script_len(&expected_prev_state, &program_id, ZkTag::R0Succinct);
    let expected_prev_redeem = build_redeem_script(
        &expected_prev_state,
        &expected_prev_seq,
        expected_len,
        &program_id,
        ZkTag::R0Succinct,
    );
    assert_eq!(settlement.prev_redeem, expected_prev_redeem);
    assert_eq!(settlement.prev_redeem.len(), settlement.next_redeem.len());

    scheduler.shutdown();
}
