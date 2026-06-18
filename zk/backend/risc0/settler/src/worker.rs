//! Drives the settlement loop, popping each bundle the aggregate prover publishes and landing it on
//! L1: see [`run`]. The per-worker [`config`] selects the redeem variant and submission policy,
//! [`confirm`] waits on covenant UTXOs, and [`settle`] builds and submits each bundle's settlement.

mod config;
mod confirm;
mod settle;

#[cfg(feature = "test-utils")]
pub use config::AlternationPacer;
pub use config::{SettlementMode, SettlementWorkerConfig};
use confirm::{ADOPT_MAX_POLLS, ConfirmOutcome, OutpointAt, confirm_outpoint, poll_outpoint};
use settle::{SettleOutcome, settle_one};
use vprogs_core_atomics::{AsyncQueue, AtomicAsyncLatch};
use vprogs_zk_aggregate_prover::{ScheduledBundle, SettlementArtifact};
use vprogs_zk_backend_risc0_api::Receipt;

use crate::covenant::{CovenantState, covenant_from_settlement};

/// Drives the settlement loop, popping each bundle the aggregate prover publishes onto `queue`.
///
/// The aggregate prover publishes every formed bundle as a [`ScheduledBundle`] handle; the worker
/// pops one, awaits its proved artifact, and (when it carries a settlement) builds a
/// [`Settlement`] in the configured [`mode`](SettlementMode), submits it, and waits for its
/// continuation UTXO to confirm before taking the next. No-op bundles (resolved with no artifact)
/// are skipped. Handles are processed one at a time, so settlements are serialized.
///
/// Runs until `shutdown` opens: every park and poll (the queue pop, the artifact wait, the
/// confirmation polling) is a biased `select!` that checks `shutdown` first, so a teardown request
/// returns promptly instead of blocking on a latch or a 1s poll. It otherwise exits only by
/// panicking on a rejected settlement or a confirmation timeout (propagated through its
/// `JoinHandle`).
pub async fn run(
    queue: AsyncQueue<ScheduledBundle<SettlementArtifact<Receipt>>>,
    cfg: SettlementWorkerConfig,
    covenant: CovenantState,
    shutdown: AtomicAsyncLatch,
) {
    let mut cov = covenant;

    // Confirm the bootstrap UTXO before chaining, so the first settlement can spend it and we know
    // its DAA score.
    let target = OutpointAt { spk: &cov.spk, outpoint: cov.outpoint };
    let Some(daa_score) = confirm_outpoint(&cfg.client, &cfg.params, target, &shutdown).await
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

        // A competing settler may have advanced the covenant since our last settlement. If our
        // in-memory covenant no longer matches this bundle's proving base, adopt the competitor's
        // settlement carried as `latest_settlement` and confirm its continuation UTXO so we can
        // spend it.
        //
        // Adoption is forward-only: `latest_settlement` is a snapshot taken at bundle formation, so
        // an older queued bundle can carry a settlement *behind* an already-advanced `cov`.
        // That snapshot reconstructs an already-spent continuation outpoint, which the node
        // never reports as an unspent UTXO, so confirmation times out: skip the bundle
        // rather than poll out the ceiling or regress `cov`. A confirmation whose DAA score
        // does not advance past our current tip is likewise not ahead, so it is ignored.
        // Only a settlement whose continuation UTXO is live and strictly ahead advances
        // `cov`, tracking the on-chain tip so a later bundle that chains from it settles
        // instead of leaving us permanently stuck behind.
        if cov.state != artifact.prev_state {
            if let Some(s) = bundle.latest_settlement() {
                if s.new_state != cov.state {
                    let adopted =
                        covenant_from_settlement(cfg.mode, &cfg.backend, &cfg.lane_key, &cov, &s);
                    let target = OutpointAt { spk: &adopted.spk, outpoint: adopted.outpoint };
                    match poll_outpoint(
                        &cfg.client,
                        &cfg.params,
                        target,
                        &shutdown,
                        ADOPT_MAX_POLLS,
                    )
                    .await
                    {
                        ConfirmOutcome::Confirmed(daa_score) if daa_score >= cov.daa_score => {
                            cov = CovenantState { daa_score, ..adopted };
                            log::info!(
                                "settlement-worker: adopted external settlement {} (covenant advanced)",
                                s.tx_id,
                            );
                        }
                        ConfirmOutcome::Confirmed(_) | ConfirmOutcome::Timeout => {
                            log::info!(
                                "settlement-worker: skipping stale external settlement {} \
                                 (not ahead of current covenant tip)",
                                s.tx_id,
                            );
                            continue;
                        }
                        ConfirmOutcome::Shutdown => break,
                    }
                }
            }
        }

        // If the base still mismatches after adopting the tip, a competitor already covered this
        // bundle's range: it is superseded. Skip it rather than asserting in the builder; a later
        // bundle chaining from the adopted tip settles.
        if cov.state != artifact.prev_state {
            log::info!(
                "settlement-worker: skipping superseded bundle (a competitor covered its range)"
            );
            continue;
        }

        // A shutdown during confirmation aborts the chain: the settlement is already on chain, but
        // we stop advancing rather than poll through teardown (a restart re-bootstraps).
        // Test-only: wait our turn so competing settlers alternate rather than one sweeping the
        // ranges. Production leaves this `None` and settles as soon as a bundle is ready.
        #[cfg(feature = "test-utils")]
        if let Some((me, pacer)) = &cfg.alternation {
            pacer.await_turn(*me, &shutdown).await;
            if shutdown.is_open() {
                break;
            }
        }
        match settle_one(&cfg, &cov, &artifact, &shutdown).await {
            SettleOutcome::Advanced(next) => {
                cov = next;
                #[cfg(feature = "test-utils")]
                if let Some((me, pacer)) = &cfg.alternation {
                    pacer.mark_settled(*me);
                }
            }
            // A competitor's settlement is already in the mempool spending this covenant outpoint,
            // so ours can never land. Hold `cov` and drop the bundle: once that
            // settlement confirms, the bridge surfaces it as `latest_settlement` and
            // the reconcile block above adopts it.
            SettleOutcome::Superseded => continue,
            SettleOutcome::Shutdown => break,
        }
    }
    log::info!("settlement-worker: shut down");
}
