//! Milestone 3: drive the deposit/transfer/withdraw runtime through the batch proving + settlement
//! pipeline, and verify the settlement journal carries both the deposit-address commitment
//! (`deposit_spk_hash`) and the permission-tree exit commitment (`permission_spk_hash`) produced by
//! a withdrawal. This is "deposits + withdraw of permission-tree leaves, through to a settlement".
//!
//! In dev mode (`RISC0_DEV_MODE=1`) the guest executes for real but the proof is faked, so the
//! on-chain `OpZkPrecompile` script check (which needs a real CUDA proof) is skipped; we assert the
//! settlement journal contents and the host-built settlement transaction structure. The real
//! on-chain landing is covered by the CUDA path of `settlement_e2e.rs` / `settlement_l1_e2e.rs`.

use std::time::Instant;

use kaspa_consensus_core::{
    network::{NetworkId, NetworkType},
    tx::{CovenantBinding, TransactionOutpoint},
};
use kaspa_hashes::Hash;
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_txscript::standard::pay_to_script_hash_script;
use tempfile::TempDir;
use vprogs_core_codec::Reader;
use vprogs_l1_types::ChainBlockMetadata;
use vprogs_node_test_utils::L1Node;
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler};
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_backend_risc0_api::{Backend, ProofType, delegate_entry_spk_hash};
use vprogs_zk_backend_risc0_covenant::{
    CommonPins, DEFAULT_PERMISSION_OUTPUT_VALUE, JOURNAL_SIZE, RedeemPins, Settlement,
    SettlementInput, SettlementWitness, StateTransition, SuccinctPins, SuccinctWitness,
    build_redeem_script, permission_spk, redeem_script_len,
};
use vprogs_zk_backend_risc0_test_suite::{
    L1TransactionExt, aggregate_batches, batch_aggregator_elf, batch_processor_elf,
    dev_mode_enabled,
    runtime_flow::{
        EXAMPLE_DEPOSIT_COVENANT_ID, RuntimeSigner, deposit_tx, init_config_tx, withdraw_tx,
    },
    runtime_processor_elf, test_lane_key,
};
use vprogs_zk_batch_prover::{Backend as _, BatchProverConfig};
use vprogs_zk_vm::{ProvingPipeline, Vm};

/// Builds a `ChainBlockMetadata` from a real simnet block (the bundling prover's
/// `get_seq_commit_lane_proof` resolves only for blocks that exist on the simnet).
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

#[tokio::test(flavor = "multi_thread")]
async fn deposit_and_withdraw_settle_with_deposit_and_permission_commitments() {
    let cov = EXAMPLE_DEPOSIT_COVENANT_ID;
    let owner = RuntimeSigner::genesis();
    let alice = RuntimeSigner::user(1);

    let temp_dir = TempDir::new().unwrap();
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
    let backend = Backend::new(
        &runtime_processor_elf(),
        &batch_processor_elf(),
        &batch_aggregator_elf(),
        ProofType::Succinct,
    );

    let l1 = L1Node::new(NetworkId::new(NetworkType::Simnet), None).await;
    let block_hashes = l1.mine_blocks(1).await;

    let config = BatchProverConfig {
        lane_key: test_lane_key(),
        covenant_id: Hash::default(),
        // The batch credits a deposit paid to the example policy's address, so the pin must be the
        // covenant's delegate-entry hash; the no-deposit sentinel would fail the carry check.
        deposit_spk_hash: delegate_entry_spk_hash(&cov),
    };
    let vm =
        Vm::new(backend.clone(), ProvingPipeline::batch(backend.clone(), storage.clone(), config));
    let mut scheduler = Scheduler::new(
        ExecutionConfig::default().with_processor(vm),
        StorageConfig::default().with_store(storage.clone()),
    );

    // One batch: Init the config, deposit to Alice, then Alice withdraws (emits a permission-tree
    // exit). The scheduler orders the dependent txs by their shared config/Alice resources.
    let metadata = metadata_for_block(&l1, block_hashes[0]).await;
    let t = Instant::now();
    let batch = scheduler.schedule(
        metadata,
        vec![
            init_config_tx(100, cov, &owner).into_scheduler_tx(0),
            deposit_tx(cov, &alice, 5_000).into_scheduler_tx(1),
            withdraw_tx(&alice, 2_000, &[0xAA; 32]).into_scheduler_tx(2),
        ],
    );
    batch.wait_committed_blocking();
    batch.wait_artifact_published_blocking();
    eprintln!("[settlement] schedule->proven wall time: {:?}", t.elapsed());

    let batch_receipt = (*batch.artifact()).clone();
    let settlement_receipt = aggregate_batches(
        &backend,
        l1.grpc_client(),
        &test_lane_key(),
        block_hashes[0],
        vec![batch_receipt],
    )
    .await;

    let journal = Backend::journal_bytes(&settlement_receipt);
    assert_eq!(journal.len(), JOURNAL_SIZE, "settlement journal must be {JOURNAL_SIZE} bytes");
    let parsed = (&mut &journal[..]).array_as::<StateTransition>("state_transition").unwrap();

    // The deposit committed the covenant deposit-address hash.
    assert_eq!(
        parsed.deposit_spk_hash,
        delegate_entry_spk_hash(&cov),
        "settlement must commit the deposit address hash for the covenant",
    );
    // The withdrawal emitted an exit, so the permission-tree commitment must be non-zero.
    assert_ne!(
        parsed.permission_spk_hash, [0u8; 32],
        "a withdrawal must produce a non-zero permission_spk_hash (count==2 settlement path)",
    );
    assert_ne!(parsed.prev_state, parsed.new_state, "state must change across the batch");

    // Build the host-side settlement and assert the count==2 structure (continuation + permission
    // exit), the path a real covenant settlement takes when there are exits.
    let program_id = backend.aggregator.id;
    let tx_image_id = backend.transaction_processor.id;
    let covenant_id_hash = Hash::from_bytes(parsed.covenant_id);
    let pins = RedeemPins::Succinct(SuccinctPins {
        common: CommonPins {
            program_id: &program_id,
            tx_image_id: &tx_image_id,
            batch_image_id: &parsed.batch_image_id,
            lane_key: &parsed.lane_key,
            permission_output_value: DEFAULT_PERMISSION_OUTPUT_VALUE,
        },
    });
    let covenant_value: u64 = 10 * DEFAULT_PERMISSION_OUTPUT_VALUE;
    let settlement = Settlement::build(&SettlementInput {
        covenant_id: covenant_id_hash,
        pins,
        prev_state: &parsed.prev_state,
        prev_lane_tip: &parsed.prev_lane_tip,
        new_state: &parsed.new_state,
        new_lane_tip: &parsed.new_lane_tip,
        block_prove_to: block_hashes[0],
        prev_outpoint: TransactionOutpoint::new(Hash::from_bytes([0xCD; 32]), 0),
        value: covenant_value,
        witness: SettlementWitness::Succinct(SuccinctWitness {
            seal: &[0u8; 8],
            claim: &[0u8; 32],
            control_index: 0,
            control_digests: &[],
            deposit_spk_hash: &parsed.deposit_spk_hash,
        }),
        permission_spk_hash: &parsed.permission_spk_hash,
    });

    assert_eq!(settlement.transaction.inputs.len(), 1);
    assert_eq!(settlement.transaction.outputs.len(), 2, "exits must produce 2 covenant outputs");
    let continuation = &settlement.transaction.outputs[0];
    assert_eq!(continuation.script_public_key, pay_to_script_hash_script(&settlement.next_redeem));
    assert_eq!(continuation.value, covenant_value - DEFAULT_PERMISSION_OUTPUT_VALUE);
    assert_eq!(continuation.covenant, Some(CovenantBinding::new(0, covenant_id_hash)));
    let exit = &settlement.transaction.outputs[1];
    assert_eq!(exit.script_public_key, permission_spk(&parsed.permission_spk_hash));
    assert_eq!(exit.value, DEFAULT_PERMISSION_OUTPUT_VALUE);
    assert_eq!(exit.covenant, Some(CovenantBinding::new(0, covenant_id_hash)));

    let expected_len = redeem_script_len(&parsed.prev_state, &pins);
    let expected_prev_redeem =
        build_redeem_script(&parsed.prev_state, &parsed.prev_lane_tip, expected_len, &pins);
    assert_eq!(settlement.prev_redeem, expected_prev_redeem);
    assert_eq!(settlement.prev_redeem.len(), settlement.next_redeem.len());

    eprintln!(
        "[settlement] dev_mode={}: journal + count==2 settlement structure verified",
        dev_mode_enabled(),
    );

    scheduler.shutdown();
    l1.shutdown().await;
}
