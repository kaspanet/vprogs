//! L1 simnet integration tests for the settlement covenant.
//!
//! These tests boot a real kaspad simnet with covenants activated, submit transactions through
//! real RPC, and verify the consensus validator accepts the covenant bootstrap and settlement
//! transactions end-to-end.
//!
//! Dev mode (`RISC0_DEV_MODE=1`, set via `.cargo/config.toml`) keeps the ZK proving fast.

use std::time::Duration;

use kaspa_consensus_core::{config::params::ForkActivation, tx::Transaction};
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_txscript::{standard::pay_to_script_hash_script, zk_precompiles::tags::ZkTag};
use vprogs_core_smt::EMPTY_HASH;
use vprogs_node_test_utils::L1Node;
use vprogs_zk_backend_risc0_test_suite::{
    batch_processor_elf, settlement_processor_elf, transaction_processor_elf,
};
use vprogs_zk_covenant::{build_redeem_script, redeem_script_len};

const TEST_COVENANT_VALUE: u64 = 100_000_000;

/// Boots a simnet with covenants enabled, builds an initial covenant redeem script whose
/// `program_id` points at the real batch-processor image, submits a bootstrap transaction, mines
/// it, and asserts that the resulting UTXO carries the covenant id the builder computed.
#[tokio::test(flavor = "multi_thread")]
async fn covenant_bootstrap_is_accepted_on_simnet() {
    let l1 = L1Node::new(Some(|p| {
        p.blockrate.coinbase_maturity = 1;
        p.covenants_activation = ForkActivation::always();
    }))
    .await;

    // Mine enough blocks so at least one coinbase UTXO matures.
    l1.mine_utxos(1).await;

    // Build a redeem script pinned to a realistic image id. We use the settlement guest's id here
    // because that's what a real on-chain covenant would verify against, even though we don't run
    // the full settlement spend in this test.
    let tx_elf = transaction_processor_elf();
    let batch_elf = batch_processor_elf();
    let settlement_elf = settlement_processor_elf();
    let backend =
        vprogs_zk_backend_risc0_api::Backend::new(&tx_elf, &batch_elf, Some(&settlement_elf));
    let program_id = *backend.settlement_image_id().expect("settlement image id");

    let initial_state = EMPTY_HASH;
    let initial_seq = [0u8; 32];
    let redeem_len = redeem_script_len(&initial_state, &program_id, ZkTag::R0Succinct);
    let redeem = build_redeem_script(
        &initial_state,
        &initial_seq,
        redeem_len,
        &program_id,
        ZkTag::R0Succinct,
    );
    let expected_spk = pay_to_script_hash_script(&redeem);

    let (bootstrap_tx, covenant_id) =
        l1.build_covenant_bootstrap_transaction(&redeem, TEST_COVENANT_VALUE).await;

    let bootstrap_tx_id = bootstrap_tx.id();

    // Mine the bootstrap into its own chain block. Kaspa DAG consensus requires a follow-up block
    // for the tx to be "accepted" by the virtual selected chain.
    let block_hash = l1.mine_block(Some(&[bootstrap_tx])).await;
    l1.mine_blocks(1).await;

    // Wait briefly for the daemon to update its UTXO index.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Fetch the bootstrap block and confirm our tx lands with the expected SPK + covenant
    // binding. Acceptance is implicit: if the consensus validator had rejected the genesis
    // covenant id, `mine_block` above would have failed.
    let accepted = l1
        .grpc_client()
        .get_block(block_hash, true)
        .await
        .expect("fetching the bootstrap block should succeed");
    let tx = accepted
        .transactions
        .iter()
        .find(|t| {
            Transaction::try_from((*t).clone()).map(|tx| tx.id()).ok() == Some(bootstrap_tx_id)
        })
        .expect("bootstrap tx must appear in its block");

    assert_eq!(tx.outputs.len(), 1);
    assert_eq!(tx.outputs[0].script_public_key, expected_spk);

    let binding =
        tx.outputs[0].covenant.as_ref().expect("bootstrap output must carry a covenant binding");
    assert_eq!(binding.0.covenant_id, covenant_id, "covenant id must match computed value");
    assert_eq!(binding.0.authorizing_input, 0);

    l1.shutdown().await;
}
