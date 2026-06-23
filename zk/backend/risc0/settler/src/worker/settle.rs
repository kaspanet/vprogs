//! Settling one proven bundle: building the settlement in the configured mode, submitting it with
//! fee-UTXO retries, classifying node rejections, and confirming the continuation UTXO.

use std::{collections::HashSet, time::Duration};

use kaspa_consensus_core::tx::TransactionOutpoint;
use kaspa_rpc_core::RpcError;
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_l1_wallet::Wallet;
use vprogs_zk_aggregate_prover::SettlementArtifact;
use vprogs_zk_backend_risc0_api::Receipt;

use super::{
    config::SettlementWorkerConfig,
    confirm::{CovenantLiveness, OutpointAt, confirm_outpoint, covenant_liveness},
};
use crate::covenant::{CovenantState, build_settlement_for_mode};

/// The result of attempting to settle one bundle.
#[allow(clippy::large_enum_variant)]
pub(super) enum SettleOutcome {
    /// The settlement landed and confirmed; the covenant advanced to this state.
    Advanced(CovenantState),
    /// A competitor already spent this covenant outpoint (its settlement is in the mempool), so
    /// this bundle is superseded. The worker keeps its covenant and waits to adopt the
    /// competitor's settlement once it confirms.
    Superseded,
    /// `shutdown` opened mid-confirmation; the worker should stop.
    Shutdown,
}

/// Builds the settlement for one proven bundle, submits it, waits for the continuation UTXO, and
/// returns the advanced covenant. Returns [`SettleOutcome::Superseded`] when a competitor's
/// settlement already spends this covenant outpoint in the mempool, or [`SettleOutcome::Shutdown`]
/// if `shutdown` opens while waiting for the continuation UTXO to confirm.
pub(super) async fn settle_one(
    cfg: &SettlementWorkerConfig,
    cov: &CovenantState,
    artifact: &SettlementArtifact<Receipt>,
    shutdown: &AtomicAsyncLatch,
) -> SettleOutcome {
    // Build the settlement and its covenant compute budget from the bundle's authoritative bounds.
    // Asserts the live covenant agrees before we pay to submit.
    let built = build_settlement_for_mode(cfg.mode, &cfg.backend, &cfg.lane_key, cov, artifact);
    let covenant_entry = cov.utxo_entry();
    let wallet = Wallet::new(&cfg.client, &cfg.params, cfg.keypair);

    // The node can reject a settlement for a transient reason that funding the fee from a
    // different UTXO resolves (see [`classify_rejection`]). Re-fund from another settled
    // UTXO, excluding each rejected one, until one is accepted or every spendable UTXO is
    // exhausted.
    let mut excluded = HashSet::new();
    let txid = loop {
        // Re-funding from another UTXO each rejection is the one unbounded wait in this worker;
        // bail on shutdown so teardown is not held up retrying a doomed submission to exhaustion.
        if shutdown.is_open() {
            return SettleOutcome::Shutdown;
        }
        let Some((tx, fee_outpoint)) = wallet
            .prepare_settlement_excluding(
                built.transaction.clone(),
                covenant_entry.clone(),
                built.compute_budget,
                &excluded,
            )
            .await
        else {
            panic!("settlement submit failed: every spendable fee UTXO was rejected");
        };
        // Jitter the submission so competing provers don't deterministically lose the spend race.
        if let Some(window) = &cfg.submit_jitter {
            if !window.is_empty() {
                let millis =
                    secp256k1::rand::random::<u64>() % (window.end - window.start) + window.start;
                tokio::time::sleep(Duration::from_millis(millis)).await;
            }
        }
        match wallet.submit_transaction(&tx).await {
            Ok(id) => break id,
            Err(e) => match classify_rejection(&e, cov.outpoint) {
                // The fee (collateral) UTXO double-spent: a different fee UTXO resolves it, so
                // exclude this one and retry.
                RejectionClass::FeeRetry => {
                    log::warn!(
                        "settlement-worker: fee UTXO {fee_outpoint} rejected, \
                         retrying with another UTXO: {e}"
                    );
                    excluded.insert(fee_outpoint);
                }
                // An orphan names no input, so the fee UTXO and the covenant input are both
                // candidates. The fee UTXO orphans transiently (a different one resolves it), but a
                // covenant outpoint a competitor already confirmed-spent orphans every submission
                // no matter the fee UTXO. Re-poll the covenant to tell them apart:
                // gone means a competitor landed first and the bundle is
                // superseded; still live means a fee orphan to retry.
                RejectionClass::Orphan => {
                    match covenant_liveness(&cfg.client, &cfg.params, cov, shutdown).await {
                        CovenantLiveness::Unspent => {
                            log::warn!(
                                "settlement-worker: fee UTXO {fee_outpoint} orphaned, \
                                 retrying with another UTXO: {e}"
                            );
                            excluded.insert(fee_outpoint);
                        }
                        CovenantLiveness::Spent => {
                            log::info!(
                                "settlement-worker: covenant outpoint {} spent by a competitor; \
                                 skipping superseded bundle",
                                cov.outpoint,
                            );
                            return SettleOutcome::Superseded;
                        }
                        CovenantLiveness::Shutdown => return SettleOutcome::Shutdown,
                    }
                }
                // A competitor's settlement already spends our covenant (state) outpoint in the
                // mempool: no fee UTXO can rescue this submission, so abandon the bundle. The
                // worker holds its covenant and adopts the competitor's advance
                // once it confirms.
                RejectionClass::Superseded => {
                    log::info!(
                        "settlement-worker: covenant outpoint {} already spent by a competitor's \
                         mempool settlement; skipping superseded bundle",
                        cov.outpoint,
                    );
                    return SettleOutcome::Superseded;
                }
                // Any other rejection is the on-chain script refusing the settlement
                // (`OpZkPrecompile` in production, the seq-commit anchor in dev); surface it
                // loudly, as that is exactly the end-to-end check this path exists
                // to make.
                RejectionClass::Fatal => panic!("settlement submit rejected by node: {e}"),
            },
        }
    };
    log::info!(
        "settlement-worker: submitted settlement {txid} (block {})",
        artifact.block_prove_to
    );

    let continuation = OutpointAt {
        spk: built.advance.continuation_spk(),
        outpoint: TransactionOutpoint::new(txid, 0),
    };
    let Some(daa_score) = confirm_outpoint(&cfg.client, &cfg.params, continuation, shutdown).await
    else {
        return SettleOutcome::Shutdown;
    };
    log::info!("settlement-worker: settlement {txid} confirmed (daa {daa_score})");

    SettleOutcome::Advanced(built.advance.apply(txid, daa_score))
}

/// How a submit rejection should be handled, keyed on which input the node is rejecting.
enum RejectionClass {
    /// The fee (collateral) input double-spent; refunding from a different UTXO resolves it.
    FeeRetry,
    /// The node orphaned the settlement: an input is missing, but the message names neither. The
    /// caller re-polls the covenant to tell a transient fee orphan (retry) from a competitor-spent
    /// covenant (superseded).
    Orphan,
    /// The covenant (state) input is already spent by a competitor's mempool settlement; this
    /// bundle is superseded and no fee UTXO can rescue it.
    Superseded,
    /// The node refused the settlement itself (the on-chain script); surface it.
    Fatal,
}

/// Classifies a settlement submit rejection by which input the node is complaining about.
///
/// The mempool reports a double-spend as `output (txid, index) already spent by transaction ... in
/// the mempool`, citing the conflicting input as the outpoint's [`Display`] form. When that input
/// is our `covenant_outpoint`, a competitor's settlement landed first and this bundle is
/// [`Superseded`](RejectionClass::Superseded); when it is any other input, the fee UTXO clashed
/// with an unconfirmed spend and a different one resolves it
/// ([`FeeRetry`](RejectionClass::FeeRetry)).
///
/// An orphan rejection (`transaction ... is an orphan where orphan is disallowed`) names only the
/// tx id, not the missing input. Both the fee UTXO (orphaned transiently) and the covenant input (a
/// competitor confirmed-spent it) can be the cause, so it maps to
/// [`Orphan`](RejectionClass::Orphan) for the caller to disambiguate by re-polling covenant
/// liveness.
///
/// Matched on message text because the wRPC layer exposes no structured rejection reason.
fn classify_rejection(e: &RpcError, covenant_outpoint: TransactionOutpoint) -> RejectionClass {
    let msg = e.to_string().to_lowercase();
    let cites_covenant_input = msg.contains(&format!("{covenant_outpoint}").to_lowercase());
    if msg.contains("already spent") {
        if cites_covenant_input { RejectionClass::Superseded } else { RejectionClass::FeeRetry }
    } else if msg.contains("orphan") {
        RejectionClass::Orphan
    } else {
        RejectionClass::Fatal
    }
}
