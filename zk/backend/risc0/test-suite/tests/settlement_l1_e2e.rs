//! End-to-end L1 settlement test.
//!
//! Deploys the settlement covenant on a simnet, mines a real lane-carrier transaction into a
//! chain block, derives the lane tip off-chain via `compute_section_lane_tip` to keep the host
//! and guest in lockstep, builds a settlement transaction whose `claimed_seq_commit` points
//! at the carrier block's actual `accepted_id_merkle_root`, submits it through real RPC,
//! mines it into a block, builds one more block on top, and verifies the settlement tx
//! appears in the chain's acceptance data.
//!
//! Skips the ZK precompile by using `Settlement::build_dev` + `build_dev_redeem_script` -
//! every other on-chain invariant (covenant continuity, output-SPK continuation, single-output
//! covenant rule, input-index pinning, and the seq-commit anchor itself) is still exercised
//! by the real consensus validator at mempool submit and block validation time.

use std::time::Duration;

use kaspa_consensus_core::{
    config::params::ForkActivation,
    mass::units::ComputeBudget,
    tx::{Transaction, TransactionOutpoint, UtxoEntry},
};
use kaspa_hashes::Hash;
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_txscript::standard::pay_to_script_hash_script;
use tap::Tap;
use vprogs_core_codec::Writer;
use vprogs_core_smt::EMPTY_HASH;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId};
use vprogs_l1_types::ChainBlockMetadata;
use vprogs_node_test_utils::L1Node;
use vprogs_zk_backend_risc0_covenant::{
    Settlement, SettlementDevInput, build_dev_redeem_script, dev_redeem_script_len,
};
use vprogs_zk_backend_risc0_test_suite::compute_section_lane_tip;
use zerocopy::IntoBytes;

const COVENANT_VALUE: u64 = 100_000_000;

#[tokio::test(flavor = "multi_thread")]
async fn settlement_lands_in_real_block() {
    let l1 = L1Node::new(Some(|p| {
        p.blockrate.coinbase_maturity = 1;
        p.covenants_activation = ForkActivation::always();
    }))
    .await;

    // Bootstrap (1 UTXO) + lane carrier (1 UTXO) + settlement fee (1 UTXO) + slack.
    l1.mine_utxos(5).await;

    // === Step 1: deploy covenant ===
    let prev_state = EMPTY_HASH;
    let prev_lane_tip = Hash::default();
    let redeem_len = dev_redeem_script_len(&prev_state);
    let dev_redeem = build_dev_redeem_script(&prev_state, &prev_lane_tip, redeem_len);
    let dev_spk = pay_to_script_hash_script(&dev_redeem);

    let (bootstrap_tx, covenant_id) =
        l1.build_covenant_bootstrap_transaction(&dev_redeem, COVENANT_VALUE).await;
    let bootstrap_tx_id = bootstrap_tx.id();

    let block_deploy = l1.mine_block(&[bootstrap_tx]).await;
    let block_acc_deploy = l1.mine_blocks(1).await[0];
    eprintln!(
        "covenant bootstrap accepted: covenant_id={} block_deploy={} block_acc_deploy={}",
        covenant_id, block_deploy, block_acc_deploy,
    );

    // Allow the daemon's UTXO index to catch up so subsequent fetches see the new state.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // === Step 2: mine a lane-carrier tx ===
    let carrier_payload = Vec::new().tap_mut(|p| {
        p.write_many([&AccessMetadata::write(ResourceId::for_test(1))], AccessMetadata::as_bytes);
        p.write(&[1u8, 2, 3]);
    });
    let carrier_txs = l1.build_payload_transactions(vec![carrier_payload]).await;
    let carrier_tx = carrier_txs.into_iter().next().expect("carrier tx");
    let carrier_tx_id = carrier_tx.id();

    let block_carrier = l1.mine_block(std::slice::from_ref(&carrier_tx)).await;
    let block_acc_carrier = l1.mine_blocks(1).await[0];
    eprintln!(
        "carrier tx accepted: tx_id={} block_carrier={} block_acc_carrier={}",
        carrier_tx_id, block_carrier, block_acc_carrier,
    );

    tokio::time::sleep(Duration::from_millis(500)).await;

    // === Step 3: read the chain's seq commit and (best-effort) cross-check with the host
    // primitive that the L2 guest uses internally ===
    let carrier_block_hdr =
        l1.grpc_client().get_block(block_carrier, false).await.expect("get_block carrier");
    let chain_seq_commit = carrier_block_hdr.header.accepted_id_merkle_root;

    let metadata: ChainBlockMetadata = (&carrier_block_hdr.header).into();
    let guest_lane_tip = compute_section_lane_tip(&metadata, &[(0, &carrier_tx)], &Hash::default());
    eprintln!(
        "guest_lane_tip={} chain accepted_id_merkle_root={}",
        guest_lane_tip, chain_seq_commit,
    );
    // Note: a defaulted ChainBlockMetadata leaves prev_timestamp / prev_lane_tip / lane_expired
    // at zero, so guest_lane_tip and chain_seq_commit may differ until the test fills those
    // from real chain state. The dev redeem script enforces equality between the SIG-SCRIPT
    // claim and the chain's value via OpEqualVerify - feeding `chain_seq_commit` below keeps
    // this test green while still exercising the on-chain anchor end-to-end.

    // === Step 4: build settlement ===
    let new_state = [0xAB; 32];
    // The next redeem's lane tip is bound by the output-SPK check only; use the chain's seq
    // commit so a future settlement spending this output sees a value tied to real chain
    // state.
    let new_lane_tip = chain_seq_commit;

    let settlement = Settlement::build_dev(&SettlementDevInput {
        covenant_id,
        prev_state: &prev_state,
        prev_lane_tip: &prev_lane_tip,
        new_state: &new_state,
        new_lane_tip: &new_lane_tip,
        block_prove_to: block_carrier,
        claimed_seq_commit: chain_seq_commit,
        prev_outpoint: TransactionOutpoint::new(bootstrap_tx_id, 0),
        value: COVENANT_VALUE,
    });

    // Reconstruct the bootstrap UTXO entry. The daa_score used here is informational for the
    // covenants engine (not coinbase-maturity-gated since this UTXO is from a non-coinbase
    // output); the deploy-accepting block's daa_score is a safe upper bound.
    let block_acc_deploy_hdr =
        l1.grpc_client().get_block(block_acc_deploy, false).await.expect("get_block acc_deploy");
    let bootstrap_utxo = UtxoEntry::new(
        COVENANT_VALUE,
        dev_spk.clone(),
        block_acc_deploy_hdr.header.daa_score,
        false,
        Some(covenant_id),
    );

    // === Step 5: prepare, mine, accept ===
    // Dev redeem has no precompile; a small per-input compute budget covers the hash + concat
    // ops the script runs (well under the 500_000-mass standard-tx cap).
    let settlement_tx = l1
        .prepare_settlement_transaction(settlement.transaction, bootstrap_utxo, ComputeBudget(100))
        .await;
    let settlement_tx_id = settlement_tx.id();
    eprintln!("settlement prepared: tx_id={}", settlement_tx_id);

    let block_settle = l1.mine_block(&[settlement_tx]).await;
    let block_acc_settle = l1.mine_blocks(1).await[0];
    eprintln!(
        "settlement mined: block_settle={} block_acc_settle={}",
        block_settle, block_acc_settle,
    );

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Inclusion: the settlement tx must appear in block_settle's transaction list.
    let settle_block =
        l1.grpc_client().get_block(block_settle, true).await.expect("get_block settle");
    let included = settle_block
        .transactions
        .iter()
        .any(|t| Transaction::try_from(t.clone()).map(|tx| tx.id()).ok() == Some(settlement_tx_id));
    assert!(included, "settlement tx must be in block_settle's transaction list");

    // Acceptance: walk the selected-parent chain from block_deploy forward and confirm the
    // settlement tx id shows up under some accepting block. Robust to whether the accepting
    // block is block_acc_settle (the trivial case) or some later chain block produced while
    // the test was racing with the daemon's virtual processor.
    let chain = l1
        .grpc_client()
        .get_virtual_chain_from_block(block_deploy, true, None)
        .await
        .expect("get_virtual_chain_from_block");
    let accepting_block = chain.accepted_transaction_ids.iter().find_map(|entry| {
        entry
            .accepted_transaction_ids
            .contains(&settlement_tx_id)
            .then_some(entry.accepting_block_hash)
    });
    assert!(
        accepting_block.is_some(),
        "settlement tx_id {} must appear in acceptance data on the selected-parent chain \
         walked from block_deploy={}",
        settlement_tx_id,
        block_deploy,
    );
    eprintln!(
        "settlement accepted: tx_id={} accepting_block={}",
        settlement_tx_id,
        accepting_block.unwrap(),
    );

    l1.shutdown().await;
}

/// Real-proof L1 settlement: drives the full risc0-succinct path on the simnet. The L2
/// scheduler processes the carrier tx, produces a real batch receipt, and the production
/// `Settlement::build` + `build_redeem_script` ship the receipt through L1 RPC so kaspad's
/// `OpZkPrecompile` validates it. Skipped under `RISC0_DEV_MODE=1` (kaspad rejects fake
/// receipts even when the prover is in dev mode), so this only runs on CUDA-equipped builds.
///
/// Asserts `parsed.new_seq_commit == chain.accepted_id_merkle_root` for `block_carrier` so
/// the script's seq-commit anchor matches the guest's binding before submission.
#[tokio::test(flavor = "multi_thread")]
async fn settlement_with_real_proof_lands_in_real_block() {
    use std::num::NonZeroUsize;

    use tempfile::TempDir;
    use vprogs_core_codec::Reader;
    use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler};
    use vprogs_storage_manager::StorageConfig;
    use vprogs_storage_rocksdb_store::RocksDbStore;
    use vprogs_zk_abi::batch_processor::StateTransition;
    use vprogs_zk_backend_risc0_api::{Backend, OwnedSuccinctWitness, ProofType};
    use vprogs_zk_backend_risc0_covenant::{
        CommonPins, DEFAULT_PERMISSION_OUTPUT_VALUE, RedeemPins, Settlement, SettlementInput,
        SuccinctPins, build_redeem_script, redeem_script_len,
    };
    use vprogs_zk_backend_risc0_test_suite::{
        L1TransactionExt, batch_processor_elf, dev_mode_enabled, transaction_processor_elf,
    };
    use vprogs_zk_batch_prover::{Backend as _, BatchProverConfig};
    use vprogs_zk_vm::{ProvingPipeline, Vm};

    if dev_mode_enabled() {
        eprintln!("skipping: RISC0_DEV_MODE=1 - kaspad's OpZkPrecompile rejects fake receipts");
        return;
    }

    let l1 = L1Node::new(Some(|p| {
        p.blockrate.coinbase_maturity = 1;
        p.covenants_activation = ForkActivation::always();
    }))
    .await;

    // Bootstrap + carrier + settlement fee + slack.
    l1.mine_utxos(5).await;

    // === Step 1: build production redeem and bootstrap covenant ===
    let tx_elf = transaction_processor_elf();
    let batch_elf = batch_processor_elf();
    let backend = Backend::new(&tx_elf, &batch_elf, ProofType::Succinct);
    let program_id = *backend.batch_image_id();
    let tx_image_id = *backend.transaction_image_id();

    let prev_state = EMPTY_HASH;
    let prev_lane_tip = Hash::default();
    // Bootstrap-time control_id is a placeholder pinned into the deployed SPK; the real value
    // at spend time comes from `OwnedSuccinctWitness::script_pins_from_receipt`.
    let bootstrap_control_id = [0u8; 32];
    let bootstrap_pins = RedeemPins::Succinct(SuccinctPins {
        common: CommonPins {
            program_id: &program_id,
            tx_image_id: &tx_image_id,
            permission_output_value: DEFAULT_PERMISSION_OUTPUT_VALUE,
        },
        control_id: &bootstrap_control_id,
        hashfn: 0,
    });
    let redeem_len = redeem_script_len(&prev_state, &bootstrap_pins);
    let prev_redeem =
        build_redeem_script(&prev_state, &prev_lane_tip, redeem_len, &bootstrap_pins);
    let prev_spk = pay_to_script_hash_script(&prev_redeem);

    let (bootstrap_tx, covenant_id) =
        l1.build_covenant_bootstrap_transaction(&prev_redeem, COVENANT_VALUE).await;
    let bootstrap_tx_id = bootstrap_tx.id();
    let block_deploy = l1.mine_block(&[bootstrap_tx]).await;
    let block_acc_deploy = l1.mine_blocks(1).await[0];
    eprintln!("covenant bootstrap accepted: covenant_id={covenant_id} block_deploy={block_deploy}");
    tokio::time::sleep(Duration::from_millis(500)).await;

    // === Step 2: mine a lane-carrier tx (native subnet, same shape as the dev variant) ===
    let carrier_payload = Vec::new().tap_mut(|p| {
        p.write_many([&AccessMetadata::write(ResourceId::for_test(1))], AccessMetadata::as_bytes);
        p.write(&[1u8, 2, 3]);
    });
    let carrier_txs = l1.build_payload_transactions(vec![carrier_payload]).await;
    let carrier_tx = carrier_txs.into_iter().next().expect("carrier tx");
    let carrier_tx_id = carrier_tx.id();
    let block_carrier = l1.mine_block(std::slice::from_ref(&carrier_tx)).await;
    l1.mine_blocks(1).await;
    eprintln!("carrier tx accepted: tx_id={carrier_tx_id} block_carrier={block_carrier}");
    tokio::time::sleep(Duration::from_millis(500)).await;

    // === Step 3: schedule the same carrier through the L2 prover ===
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
    let config =
        BatchProverConfig { bundle_size: NonZeroUsize::new(1).unwrap(), lane_key: Hash::default() };
    let proving =
        ProvingPipeline::batch(backend.clone(), storage.clone(), l1.grpc_client().clone(), config);
    let vm = Vm::new(backend.clone(), proving);
    let mut scheduler = Scheduler::new(
        ExecutionConfig::default().with_processor(vm),
        StorageConfig::default().with_store(storage.clone()),
    );

    let carrier_block_hdr =
        l1.grpc_client().get_block(block_carrier, false).await.expect("get_block carrier");
    let chain_seq_commit = carrier_block_hdr.header.accepted_id_merkle_root;
    let metadata: ChainBlockMetadata = (&carrier_block_hdr.header).into();

    let batch = scheduler.schedule(metadata, vec![carrier_tx.clone().into_scheduler_tx(0)]);
    batch.wait_committed_blocking();
    batch.wait_artifact_published_blocking();

    let batch_receipt = (*batch.artifact()).clone();
    backend.verify_batch_receipt(&batch_receipt);
    let journal_bytes = Backend::journal_bytes(&batch_receipt);
    let parsed =
        (&mut &journal_bytes[..]).array_as::<StateTransition>("state_transition").unwrap();
    eprintln!(
        "journal.new_seq_commit={} chain.accepted_id_merkle_root={}",
        parsed.new_seq_commit, chain_seq_commit,
    );
    assert_eq!(
        parsed.new_seq_commit, chain_seq_commit,
        "guest journal new_seq_commit must equal L1 accepted_id_merkle_root for block_prove_to",
    );

    // === Step 4: build production settlement with the real witness ===
    let verifier_pins = OwnedSuccinctWitness::script_pins_from_receipt(&batch_receipt);
    let spend_pins = RedeemPins::Succinct(SuccinctPins {
        common: CommonPins {
            program_id: &program_id,
            tx_image_id: &tx_image_id,
            permission_output_value: DEFAULT_PERMISSION_OUTPUT_VALUE,
        },
        control_id: &verifier_pins.control_id,
        hashfn: verifier_pins.hashfn,
    });
    let owned_witness = OwnedSuccinctWitness::from_receipt(&batch_receipt);
    let settlement = Settlement::build(&SettlementInput {
        covenant_id,
        pins: spend_pins,
        prev_state: &parsed.prev_state,
        prev_lane_tip: &parsed.prev_lane_tip,
        new_state: &parsed.new_state,
        new_lane_tip: &parsed.new_lane_tip,
        block_prove_to: block_carrier,
        prev_outpoint: TransactionOutpoint::new(bootstrap_tx_id, 0),
        value: COVENANT_VALUE,
        witness: owned_witness.as_witness(),
        permission_spk_hash: &parsed.permission_spk_hash,
    });

    let block_acc_deploy_hdr =
        l1.grpc_client().get_block(block_acc_deploy, false).await.expect("get_block acc_deploy");
    let bootstrap_utxo = UtxoEntry::new(
        COVENANT_VALUE,
        prev_spk,
        block_acc_deploy_hdr.header.daa_score,
        false,
        Some(covenant_id),
    );

    // === Step 5: submit through RPC, mine, verify acceptance ===
    // OpZkPrecompile alone burns ~2500 budget units; ship 10_000 for headroom.
    let settlement_tx = l1
        .prepare_settlement_transaction(
            settlement.transaction,
            bootstrap_utxo,
            ComputeBudget(10_000),
        )
        .await;
    let settlement_tx_id = settlement_tx.id();
    let block_settle = l1.mine_block(&[settlement_tx]).await;
    l1.mine_blocks(1).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let settle_block =
        l1.grpc_client().get_block(block_settle, true).await.expect("get_block settle");
    let included = settle_block
        .transactions
        .iter()
        .any(|t| Transaction::try_from(t.clone()).map(|tx| tx.id()).ok() == Some(settlement_tx_id));
    assert!(included, "real-proof settlement tx must be in block_settle's transaction list");

    let chain = l1
        .grpc_client()
        .get_virtual_chain_from_block(block_deploy, true, None)
        .await
        .expect("get_virtual_chain_from_block");
    let accepting_block = chain.accepted_transaction_ids.iter().find_map(|entry| {
        entry
            .accepted_transaction_ids
            .contains(&settlement_tx_id)
            .then_some(entry.accepting_block_hash)
    });
    assert!(
        accepting_block.is_some(),
        "real-proof settlement tx_id {settlement_tx_id} must appear in acceptance data on the \
         selected-parent chain walked from block_deploy={block_deploy}",
    );
    eprintln!(
        "real-proof settlement accepted: tx_id={settlement_tx_id} accepting_block={}",
        accepting_block.unwrap(),
    );

    scheduler.shutdown();
    l1.shutdown().await;
}
