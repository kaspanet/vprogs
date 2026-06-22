//! Drives the settlement loop, popping each bundle the aggregate prover publishes and landing it on
//! L1: see [`run`]. The per-worker [`config`] selects the redeem variant and submission policy,
//! [`confirm`] waits on covenant UTXOs, and [`settle`] builds and submits each bundle's settlement.

mod config;
mod confirm;
mod settle;

#[cfg(feature = "test-utils")]
pub use config::AlternationPacer;
pub use config::{SettlementMode, SettlementWorkerConfig};
use confirm::{OutpointAt, confirm_outpoint};
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

    // Establish the starting covenant tip before chaining, reading it SOLELY from the live
    // settlement handle the bridge writes - never by scanning L1. The bridge replays the chain
    // from the deploy block and publishes the tip's `last_settlement` into the handle, so when the
    // covenant has already advanced the handle carries the canonical continuation; reconstruct the
    // tip from it directly (outpoint `tx_id:0`, SPK rebuilt from the seeded redeem) without an
    // on-chain confirm. This is the exact `last_settlement` a chain scan would derive, with no RPC.
    //
    // When the handle is empty (a fresh deploy, or the bridge has not yet replayed a settlement),
    // the in-memory `cov` already points at the bootstrap. A `start_from` is the resume / catch-up
    // signal: that bootstrap may already be spent, so it must not be hard-confirmed (which would
    // time out and panic). Instead leave `cov` at the bootstrap and let the loop's mid-stream
    // adoption advance it once the bridge publishes the live tip - a catch-up prover's first
    // bundle proves from the already-advanced on-chain state, mismatches the empty bootstrap, and
    // adopts the handle. A fresh deploy (`start_from` unset) has an unspent bootstrap and confirms
    // it directly, stamping its DAA score, so the first settlement can spend it.
    if let Some(s) = cfg
        .settlement
        .as_ref()
        .and_then(|h| h.load_full())
        .filter(|s| s.new_state != cov.state && s.daa_score.get() >= cov.daa_score)
    {
        cov = covenant_from_settlement(cfg.mode, &cfg.backend, &cfg.lane_key, &cov, &s);
        log::info!(
            "settlement-worker: starting covenant {} from live settlement handle {} (tip daa {})",
            cov.covenant_id,
            s.tx_id,
            cov.daa_score,
        );
    } else if cfg.start_from.is_none() {
        // Fresh deploy: the supplied bootstrap UTXO is unspent. Confirm it before chaining so the
        // first settlement can spend it and we know its DAA score. It must confirm; its absence is
        // a real liveness failure worth the panicking confirm.
        let target = OutpointAt { spk: &cov.spk, outpoint: cov.outpoint };
        let Some(daa_score) = confirm_outpoint(&cfg.client, &cfg.params, target, &shutdown).await
        else {
            log::info!("settlement-worker: shutdown before bootstrap confirmed");
            return;
        };
        cov.daa_score = daa_score;
    }
    log::info!(
        "settlement-worker: covenant {} ready at tip (daa {})",
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
        // in-memory covenant no longer matches this bundle's proving base, consult the live
        // settlement handle the bridge writes: it carries the covenant's last settlement the bridge
        // observed in an accepted chain block, including its DAA score. When that settlement is
        // ahead of `cov`, adopt it as the new optimistic tip so a later bundle that chains from it
        // settles instead of leaving us permanently stuck behind.
        //
        // The handle is read, not the per-bundle snapshot: the settler advances `cov`
        // optimistically when it settles, but the bridge needs ≈RTT to observe that, so the
        // handle lags `cov` by one settlement. Adoption is therefore gated on the handle
        // being *ahead* (`s.new_state` differs from `cov.state`) and forward-only
        // (`s.daa_score` at or past `cov.daa_score`); a handle value behind `cov` (a
        // competitor we already passed) is ignored. The continuation outpoint is adopted
        // without an on-chain confirm: the bridge only publishes settlements from accepted
        // chain blocks, so the UTXO existed, and the rare case it was already spent by a reorg/race
        // is caught at settle time (`classify_rejection` -> `Superseded`), which skips the bundle.
        if cov.state != artifact.prev_state {
            if let Some(s) = cfg.settlement.as_ref().and_then(|h| h.load_full()) {
                if s.new_state != cov.state && s.daa_score.get() >= cov.daa_score {
                    cov = covenant_from_settlement(cfg.mode, &cfg.backend, &cfg.lane_key, &cov, &s);
                    log::info!(
                        "settlement-worker: adopted external settlement {} (covenant advanced to daa {})",
                        s.tx_id,
                        cov.daa_score,
                    );
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
            // so ours can never land. Hold `cov` and drop the bundle: once that settlement confirms
            // in a chain block, the bridge publishes it to the live settlement handle and the
            // reconcile block above adopts it.
            SettleOutcome::Superseded => continue,
            SettleOutcome::Shutdown => break,
        }
    }
    log::info!("settlement-worker: shut down");
}
