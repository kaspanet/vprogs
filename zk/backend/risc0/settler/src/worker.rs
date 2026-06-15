use std::time::Duration;

use kaspa_addresses::Prefix;
use kaspa_consensus_core::{
    config::params::Params,
    tx::{ScriptPublicKey, TransactionOutpoint, UtxoEntry},
};
use kaspa_hashes::Hash;
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_txscript::standard::{extract_script_pub_key_address, pay_to_script_hash_script};
use kaspa_wrpc_client::prelude::KaspaRpcClient;
use secp256k1::Keypair;
use tokio::sync::mpsc::UnboundedReceiver;
use vprogs_l1_wallet::Wallet;
use vprogs_zk_aggregate_prover::{BundleOutcome, SettlementArtifact};
use vprogs_zk_backend_risc0_api::{Backend, OwnedSuccinctWitness, Receipt};
use vprogs_zk_backend_risc0_covenant::{Settlement, SettlementInput};

use crate::covenant::{AnchorSeqCommit, CovenantState, redeem_pins};

/// Poll cadence and ceiling for waiting on a covenant UTXO to confirm on chain.
const CONFIRM_POLL_INTERVAL: Duration = Duration::from_secs(1);
const CONFIRM_MAX_POLLS: u32 = 300;

/// Everything the settlement worker needs that isn't carried per bundle.
pub struct SettlementWorkerConfig {
    /// wRPC client for funding, submission, and confirmation polling.
    pub client: KaspaRpcClient,
    /// Consensus params (mass calc, network prefix).
    pub params: Params,
    /// Key that funds and signs settlement fees.
    pub keypair: Keypair,
    /// Lane key the covenant SPK pins.
    pub lane_key: Hash,
    /// Backend, for the covenant's redeem pins (guest image ids).
    pub backend: Backend,
}

/// Drives the settlement loop until the outcome channel closes (aggregate prover / node shutdown).
///
/// The aggregate prover reports every bundle as a [`BundleOutcome`]; the ones that carry a
/// settlement are submitted: build a production [`Settlement`], submit it, and wait for its
/// continuation UTXO to confirm before taking the next. No-op outcomes are skipped. The channel is
/// processed one item at a time, so settlements are serialized.
pub async fn run(
    mut rx: UnboundedReceiver<BundleOutcome<Receipt>>,
    cfg: SettlementWorkerConfig,
    covenant: CovenantState,
) {
    let mut cov = covenant;

    // Confirm the bootstrap UTXO before chaining, so the first settlement can spend it and we know
    // its DAA score.
    cov.daa_score = confirm_outpoint(&cfg.client, &cfg.params, &cov.spk, cov.outpoint).await;
    log::info!(
        "settlement-worker: bootstrap covenant {} confirmed (daa {})",
        cov.covenant_id,
        cov.daa_score,
    );

    while let Some(outcome) = rx.recv().await {
        // TODO: track which settlements are done vs pending and persist that (a no-op outcome marks
        // a proved-but-not-settled range), so a restart can resume mid-chain instead of
        // re-bootstrapping.
        // TODO: fee-bump a settlement that does not confirm within a deadline, rather than polling
        // `confirm_outpoint` indefinitely.
        // TODO: handle reorgs that orphan `artifact.block_prove_to` (single-miner / low-reorg
        // only).
        let Some(artifact) = outcome.settlement else {
            continue;
        };
        cov = settle_one(&cfg, cov, artifact).await;
    }
    log::info!("settlement-worker: outcome channel closed; stopping");
}

/// Builds the settlement for one proven bundle, submits it, waits for the continuation UTXO, and
/// returns the advanced covenant.
async fn settle_one(
    cfg: &SettlementWorkerConfig,
    cov: CovenantState,
    artifact: SettlementArtifact<Receipt>,
) -> CovenantState {
    // The bundle's bounds are authoritative for the on-chain script; assert the live covenant
    // agrees before paying to submit, so a mismatch fails loudly and locally.
    assert_eq!(
        artifact.prev_state, cov.state,
        "settlement prev_state must chain from the live covenant state",
    );
    assert_eq!(
        artifact.prev_lane_tip, cov.lane_tip,
        "settlement prev_lane_tip must match the spent covenant's redeem prefix",
    );
    assert_eq!(
        Hash::from_bytes(artifact.covenant_id),
        cov.covenant_id,
        "settlement covenant_id must match the live covenant",
    );

    let owned_witness = OwnedSuccinctWitness::from_receipt(&artifact.receipt);
    let settlement = Settlement::build(&SettlementInput {
        covenant_id: cov.covenant_id,
        pins: redeem_pins(&cfg.backend, &cfg.lane_key),
        prev_state: &artifact.prev_state,
        prev_lane_tip: &artifact.prev_lane_tip,
        new_state: &artifact.new_state,
        new_lane_tip: &artifact.new_lane_tip,
        block_prove_to: artifact.block_prove_to,
        prev_outpoint: cov.outpoint,
        value: cov.value,
        witness: owned_witness.as_witness(),
        permission_spk_hash: &artifact.permission_spk_hash,
    });
    let continuation_spk = pay_to_script_hash_script(&settlement.next_redeem);

    // Size the covenant input's committed compute budget from the script units it actually
    // consumes: the script engine anchors `new_seq_commit` to `block_prove_to`, so feed it the
    // bundle's value. An oversized budget inflates the tx's compute mass past the per-tx limit
    // and the node rejects it; this yields the minimal sufficient value.
    let accessor =
        AnchorSeqCommit { block: artifact.block_prove_to, seq_commit: artifact.new_seq_commit };
    let budget = settlement.covenant_compute_budget(cov.covenant_id, &accessor);

    let covenant_entry =
        UtxoEntry::new(cov.value, cov.spk.clone(), cov.daa_score, false, Some(cov.covenant_id));
    let wallet = Wallet::new(&cfg.client, &cfg.params, cfg.keypair);
    let tx =
        wallet.prepare_settlement_transaction(settlement.transaction, covenant_entry, budget).await;
    let txid = match wallet.submit_transaction(&tx).await {
        Ok(id) => id,
        // A rejection here is the on-chain script (incl. `OpZkPrecompile`) refusing the settlement:
        // surface it loudly — that is exactly the end-to-end check this path exists to make.
        Err(e) => panic!("settlement submit rejected by node: {e}"),
    };
    log::info!(
        "settlement-worker: submitted settlement {txid} (block {})",
        artifact.block_prove_to
    );

    let continuation_outpoint = TransactionOutpoint::new(txid, 0);
    let daa_score =
        confirm_outpoint(&cfg.client, &cfg.params, &continuation_spk, continuation_outpoint).await;
    log::info!("settlement-worker: settlement {txid} confirmed (daa {daa_score})");

    CovenantState {
        covenant_id: cov.covenant_id,
        state: artifact.new_state,
        lane_tip: artifact.new_lane_tip,
        outpoint: continuation_outpoint,
        spk: continuation_spk,
        value: cov.value,
        daa_score,
    }
}

/// Polls the node until `outpoint` appears at `spk`'s P2SH address, returning its block DAA score.
/// Covenant UTXOs are P2SH, so the node's utxoindex tracks them by their script address. Panics on
/// timeout (a settlement that never confirms is a liveness failure worth surfacing).
async fn confirm_outpoint(
    client: &KaspaRpcClient,
    params: &Params,
    spk: &ScriptPublicKey,
    outpoint: TransactionOutpoint,
) -> u64 {
    let prefix = Prefix::from(params.net.network_type());
    let address = extract_script_pub_key_address(spk, prefix).expect("covenant P2SH address");
    for _ in 0..CONFIRM_MAX_POLLS {
        let utxos = client
            .get_utxos_by_addresses(vec![address.clone()])
            .await
            .expect("get_utxos_by_addresses");
        if let Some(entry) =
            utxos.into_iter().find(|e| TransactionOutpoint::from(e.outpoint) == outpoint)
        {
            return entry.utxo_entry.block_daa_score;
        }
        tokio::time::sleep(CONFIRM_POLL_INTERVAL).await;
    }
    panic!("covenant outpoint {outpoint} not confirmed at {address} within timeout");
}
