use std::time::Duration;

use kaspa_addresses::Prefix;
use kaspa_consensus_core::{
    config::params::Params,
    tx::{ScriptPublicKey, TransactionOutpoint, UtxoEntry},
};
use kaspa_hashes::Hash;
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_txscript::standard::extract_script_pub_key_address;
use kaspa_wrpc_client::prelude::KaspaRpcClient;
use secp256k1::Keypair;
use vprogs_core_atomics::{AsyncQueue, AtomicAsyncLatch};
use vprogs_l1_wallet::Wallet;
use vprogs_zk_aggregate_prover::{ScheduledBundle, SettlementArtifact};
use vprogs_zk_backend_risc0_api::{Backend, Receipt};

use crate::covenant::{CovenantState, build_settlement};

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

/// Drives the settlement loop, popping each bundle the aggregate prover publishes onto `queue`.
///
/// The aggregate prover publishes every formed bundle as a [`ScheduledBundle`] handle; the worker
/// pops one, awaits its proved artifact, and (when it carries a settlement) builds a production
/// [`Settlement`], submits it, and waits for its continuation UTXO to confirm before taking the
/// next. No-op bundles (resolved with no artifact) are skipped. Handles are processed one at a
/// time, so settlements are serialized.
///
/// Runs until `shutdown` opens: every park and poll (the queue pop, the artifact wait, the
/// confirmation polling) is a biased `select!` that checks `shutdown` first, so a teardown request
/// returns promptly instead of blocking on a latch or a 1s poll. It otherwise exits only by
/// panicking on a rejected settlement or a confirmation timeout (propagated through its
/// `JoinHandle`).
pub async fn run(
    queue: AsyncQueue<ScheduledBundle<Receipt>>,
    cfg: SettlementWorkerConfig,
    covenant: CovenantState,
    shutdown: AtomicAsyncLatch,
) {
    let mut cov = covenant;

    // Confirm the bootstrap UTXO before chaining, so the first settlement can spend it and we know
    // its DAA score.
    let Some(daa_score) =
        confirm_outpoint(&cfg.client, &cfg.params, &cov.spk, cov.outpoint, &shutdown).await
    else {
        log::info!("settlement-worker: shutdown before bootstrap confirmed");
        return;
    };
    cov.daa_score = daa_score;
    log::info!(
        "settlement-worker: bootstrap covenant {} confirmed (daa {})",
        cov.covenant_id,
        cov.daa_score,
    );

    loop {
        // TODO: track which settlements are done vs pending and persist that (a no-op bundle marks
        // a proved-but-not-settled range), so a restart can resume mid-chain instead of
        // re-bootstrapping.
        // TODO: fee-bump a settlement that does not confirm within a deadline, rather than polling
        // `confirm_outpoint` indefinitely.
        // TODO: handle reorgs that orphan `artifact.block_prove_to` (single-miner / low-reorg
        // only).
        let bundle = tokio::select! {
            biased;
            () = shutdown.wait() => break,
            bundle = queue.wait_and_pop() => bundle,
        };
        // The handle is published before its proof exists; await the artifact before reading it.
        tokio::select! {
            biased;
            () = shutdown.wait() => break,
            () = bundle.wait_artifact_published() => {}
        }
        let Some(artifact) = bundle.artifact() else {
            continue;
        };
        // A shutdown during confirmation aborts the chain: the settlement is already on chain, but
        // we stop advancing rather than poll through teardown (a restart re-bootstraps).
        match settle_one(&cfg, cov, &artifact, &shutdown).await {
            Some(next) => cov = next,
            None => break,
        }
    }
    log::info!("settlement-worker: shut down");
}

/// Builds the settlement for one proven bundle, submits it, waits for the continuation UTXO, and
/// returns the advanced covenant. Returns `None` if `shutdown` opens while waiting for the
/// continuation UTXO to confirm.
async fn settle_one(
    cfg: &SettlementWorkerConfig,
    cov: CovenantState,
    artifact: &SettlementArtifact<Receipt>,
    shutdown: &AtomicAsyncLatch,
) -> Option<CovenantState> {
    // Build the settlement and its covenant compute budget from the bundle's authoritative bounds
    // (shared with the sim driver). Asserts the live covenant agrees before we pay to submit.
    let built = build_settlement(&cfg.backend, &cfg.lane_key, &cov, artifact);

    let covenant_entry =
        UtxoEntry::new(cov.value, cov.spk.clone(), cov.daa_score, false, Some(cov.covenant_id));
    let wallet = Wallet::new(&cfg.client, &cfg.params, cfg.keypair);
    let tx = wallet
        .prepare_settlement_transaction(built.transaction, covenant_entry, built.compute_budget)
        .await;
    let txid = match wallet.submit_transaction(&tx).await {
        Ok(id) => id,
        // A rejection here is the on-chain script (incl. `OpZkPrecompile`) refusing the settlement;
        // surface it loudly, as that is exactly the end-to-end check this path exists to make.
        Err(e) => panic!("settlement submit rejected by node: {e}"),
    };
    log::info!(
        "settlement-worker: submitted settlement {txid} (block {})",
        artifact.block_prove_to
    );

    let continuation_outpoint = TransactionOutpoint::new(txid, 0);
    let daa_score = confirm_outpoint(
        &cfg.client,
        &cfg.params,
        built.advance.continuation_spk(),
        continuation_outpoint,
        shutdown,
    )
    .await?;
    log::info!("settlement-worker: settlement {txid} confirmed (daa {daa_score})");

    Some(built.advance.apply(txid, daa_score))
}

/// Polls the node until `outpoint` appears at `spk`'s P2SH address, returning its block DAA score.
/// Covenant UTXOs are P2SH, so the node's utxoindex tracks them by their script address. Returns
/// `None` if `shutdown` opens while polling. Panics on timeout (a settlement that never confirms is
/// a liveness failure worth surfacing).
async fn confirm_outpoint(
    client: &KaspaRpcClient,
    params: &Params,
    spk: &ScriptPublicKey,
    outpoint: TransactionOutpoint,
    shutdown: &AtomicAsyncLatch,
) -> Option<u64> {
    let prefix = Prefix::from(params.net.network_type());
    let address = extract_script_pub_key_address(spk, prefix).expect("covenant P2SH address");
    for _ in 0..CONFIRM_MAX_POLLS {
        if shutdown.is_open() {
            return None;
        }
        let utxos = client
            .get_utxos_by_addresses(vec![address.clone()])
            .await
            .expect("get_utxos_by_addresses");
        if let Some(entry) =
            utxos.into_iter().find(|e| TransactionOutpoint::from(e.outpoint) == outpoint)
        {
            return Some(entry.utxo_entry.block_daa_score);
        }
        // Cancelable poll delay: wake on shutdown instead of sleeping out the full interval.
        tokio::select! {
            biased;
            () = shutdown.wait() => return None,
            () = tokio::time::sleep(CONFIRM_POLL_INTERVAL) => {}
        }
    }
    panic!("covenant outpoint {outpoint} not confirmed at {address} within timeout");
}
