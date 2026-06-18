//! Drives the settlement loop, popping each bundle the aggregate prover publishes and landing it on
//! L1: see [`run`]. The per-worker [`config`] selects the redeem variant and submission policy,
//! [`confirm`] waits on covenant UTXOs, and [`settle`] builds and submits each bundle's settlement.

mod config;
mod confirm;
mod resolve;
mod settle;

#[cfg(feature = "test-utils")]
pub use config::AlternationPacer;
pub use config::{SettlementMode, SettlementWorkerConfig};
use confirm::{ADOPT_MAX_POLLS, ConfirmOutcome, OutpointAt, confirm_outpoint, poll_outpoint};
use resolve::newest_settlement;
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

    // Resolve the CURRENT on-chain covenant tip before chaining. A fresh deploy (`start_from` unset)
    // starts at the supplied, unspent bootstrap and confirms it directly. A resume / catch-up into
    // an already-advanced covenant (`start_from` set) is handed a bootstrap outpoint that is now
    // spent, so confirming it would time out and panic; instead scan the selected-parent chain
    // forward from the deploy block for the newest settlement of this covenant and reconstruct the
    // live continuation state from it. This is the same `last_settlement` the bridge derives off the
    // replayed chain, so the resolved tip is exactly the one the replay rebuilt L2 state up to.
    match cfg.start_from {
        Some(start_from) => {
            let Some(daa_score) = resolve_and_confirm_tip(&cfg, &mut cov, start_from, &shutdown).await
            else {
                log::info!("settlement-worker: shutdown before tip resolved");
                return;
            };
            cov.daa_score = daa_score;
        }
        None => {
            // Confirm the supplied (unspent) bootstrap UTXO before chaining, so the first settlement
            // can spend it and we know its DAA score. It must confirm; its absence is a real
            // liveness failure worth the panicking confirm.
            let target = OutpointAt { spk: &cov.spk, outpoint: cov.outpoint };
            let Some(daa_score) = confirm_outpoint(&cfg.client, &cfg.params, target, &shutdown).await
            else {
                log::info!("settlement-worker: shutdown before bootstrap confirmed");
                return;
            };
            cov.daa_score = daa_score;
        }
    }
    log::info!(
        "settlement-worker: covenant {} confirmed at tip (daa {})",
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
                log::info!(
                    "settlement-worker: TRACE_ADOPT cov {:?} daa {} bundle.prev {:?} snap.new {:?}",
                    &cov.state[..4], cov.daa_score, &artifact.prev_state[..4], &s.new_state[..4],
                );
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
                "settlement-worker: TRACE_SKIP cov {:?} prev {:?} new {:?} daa {}",
                &cov.state[..4], &artifact.prev_state[..4], &artifact.new_state[..4], cov.daa_score,
            );
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
            log::info!("settlement-worker: TRACE_PACER me {me} awaiting turn (cov {:?})", &cov.state[..4]);
            pacer.await_turn(*me, &shutdown).await;
            log::info!("settlement-worker: TRACE_PACER me {me} turn granted");
            if shutdown.is_open() {
                break;
            }
        }
        log::info!("settlement-worker: TRACE_SETTLE attempting settle prev {:?} new {:?}", &artifact.prev_state[..4], &artifact.new_state[..4]);
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

/// Sets `cov` to the current on-chain covenant tip and confirms its UTXO, returning the confirmed
/// DAA score (or `None` on shutdown).
///
/// A covenant that has settled resolves to its newest on-chain settlement; one that has not yet
/// settled (a catch-up joining while the bootstrap is still unspent) keeps the supplied bootstrap.
/// The resolution races a live competitor whose settlement can spend the resolved outpoint (or the
/// still-unspent bootstrap) between the scan and the confirm, so the resolved tip is confirmed with
/// a bounded poll and re-resolved on timeout rather than confirmed with the panicking
/// [`confirm_outpoint`]. A competitor that settles a never-yet-settled covenant out from under the
/// scan spends the bootstrap; the re-resolve then finds that new on-chain tip and confirms it.
async fn resolve_and_confirm_tip(
    cfg: &SettlementWorkerConfig,
    cov: &mut CovenantState,
    start_from: kaspa_hashes::Hash,
    shutdown: &AtomicAsyncLatch,
) -> Option<u64> {
    let bootstrap = cov.clone();
    loop {
        // Resolve the outpoint to confirm: the newest on-chain settlement's continuation, or the
        // still-unspent bootstrap when the covenant has never settled.
        match newest_settlement(&cfg.client, cfg.covenant_id, start_from).await {
            Some(s) => {
                *cov = covenant_from_settlement(cfg.mode, &cfg.backend, &cfg.lane_key, &bootstrap, &s);
                log::info!(
                    "settlement-worker: resolved covenant {} tip from on-chain settlement {} \
                     (continuation {})",
                    cov.covenant_id,
                    s.tx_id,
                    cov.outpoint,
                );
            }
            None => {
                *cov = bootstrap.clone();
                log::info!(
                    "settlement-worker: covenant {} never settled; confirming bootstrap {}",
                    cov.covenant_id,
                    cov.outpoint,
                );
            }
        }

        // Bounded confirm: the resolved outpoint is found within a poll or two unless a competitor
        // already spent it (advanced the covenant, or settled a never-yet-settled covenant, since
        // the scan), in which case re-resolve to pick up the newer on-chain tip. A competitor race
        // here is normal liveness, never a panic.
        let target = OutpointAt { spk: &cov.spk, outpoint: cov.outpoint };
        match poll_outpoint(&cfg.client, &cfg.params, target, shutdown, ADOPT_MAX_POLLS).await {
            ConfirmOutcome::Confirmed(daa_score) => return Some(daa_score),
            ConfirmOutcome::Shutdown => return None,
            ConfirmOutcome::Timeout => log::info!(
                "settlement-worker: resolved covenant {} tip {} already spent by a competitor; \
                 re-resolving",
                cov.covenant_id,
                cov.outpoint,
            ),
        }
    }
}
