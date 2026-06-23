//! Drives the settlement loop, popping each bundle the aggregate prover publishes and landing it on
//! L1: see [`run`]. The per-worker [`config`] selects the redeem variant and submission policy; the
//! environment-agnostic [`Settler`](crate::settle::Settler) builds, funds, submits, and awaits each
//! bundle's settlement. This worker is the production driver: it constructs a `Settler` from the
//! wRPC-backed [`WalletFeeSource`]/[`RpcSink`] and runs the queue loop around it.

mod config;

#[cfg(feature = "test-utils")]
pub use config::AlternationPacer;
pub use config::{SettlementMode, SettlementWorkerConfig};
use vprogs_core_atomics::{AsyncQueue, AtomicAsyncLatch};
use vprogs_zk_aggregate_prover::{ScheduledBundle, SettlementArtifact};
use vprogs_zk_backend_risc0_api::Receipt;

use crate::{
    confirm::{OutpointAt, confirm_outpoint},
    covenant::{CovenantState, covenant_from_settlement},
    settle::{RpcSink, SettleOutcome, Settler, WalletFeeSource},
};

/// Drives the settlement loop, popping each bundle the aggregate prover publishes onto `queue`.
///
/// The aggregate prover publishes every formed bundle as a [`ScheduledBundle`] handle; the worker
/// pops one, awaits its proved artifact, and (when it carries a settlement) hands it to a
/// [`Settler`](crate::settle::Settler) to build, fund, submit, and **await** its confirmation
/// before taking the next. No-op bundles (resolved with no artifact) are skipped. Handles are
/// processed one at a time, so settlements are serialized.
///
/// Runs until `shutdown` opens: every park (the queue pop, the artifact wait, the confirmation
/// await) is a biased `select!` that checks `shutdown` first, so a teardown request returns
/// promptly instead of blocking on a latch. It otherwise exits only by panicking on a rejected
/// settlement (propagated through its `JoinHandle`).
pub async fn run(
    queue: AsyncQueue<ScheduledBundle<SettlementArtifact<Receipt>>>,
    cfg: SettlementWorkerConfig,
    covenant: CovenantState,
    shutdown: AtomicAsyncLatch,
) {
    let mut cov = covenant;

    // The production settler: fund each fee over wRPC, submit to the node's mempool, and confirm by
    // awaiting the bridge's settlement watch. The bridge fills that watch as it follows the chain.
    let settler = Settler::new(
        WalletFeeSource::new(cfg.client.clone(), cfg.params.clone(), cfg.keypair),
        RpcSink::new(
            cfg.client.clone(),
            cfg.params.clone(),
            cfg.keypair,
            cfg.submit_jitter.clone(),
        ),
        cfg.backend.clone(),
        cfg.lane_key,
        cfg.mode,
        cfg.settlement.clone(),
    );

    // Establish the starting covenant tip before chaining, reading it SOLELY from the settlement
    // watch the bridge writes - never by scanning L1. The bridge replays the chain from the deploy
    // block and publishes the tip's `last_settlement`, so when the covenant has already advanced
    // the watch carries the canonical continuation; reconstruct the tip from it directly
    // (outpoint `tx_id:0`, SPK rebuilt from the seeded redeem) without an on-chain confirm.
    // This is the exact `last_settlement` a chain scan would derive, with no RPC.
    //
    // When the watch is empty (a fresh deploy, or the bridge has not yet replayed a settlement),
    // the in-memory `cov` already points at the bootstrap. A `start_from` is the resume /
    // catch-up signal: that bootstrap may already be spent, so it must not be hard-confirmed
    // (which would time out and panic). Instead leave `cov` at the bootstrap and let the loop's
    // mid-stream adoption advance it once the bridge publishes the live tip - a catch-up
    // prover's first bundle proves from the already-advanced on-chain state, mismatches the
    // empty bootstrap, and adopts the watch. A fresh deploy (`start_from` unset) has an unspent
    // bootstrap and confirms it directly, stamping its DAA score, so the first settlement can
    // spend it.
    let initial = *cfg.settlement.borrow();
    if let Some(s) =
        initial.filter(|s| s.new_state != cov.state && s.daa_score.get() >= cov.daa_score)
    {
        cov = covenant_from_settlement(cfg.mode, &cfg.backend, &cfg.lane_key, &cov, &s);
        log::info!(
            "settlement-worker: starting covenant {} from live settlement {} (tip daa {})",
            cov.covenant_id,
            s.tx_id,
            cov.daa_score,
        );
    } else if cfg.start_from.is_none() {
        // Fresh deploy: the supplied bootstrap UTXO is unspent. Confirm it before chaining so the
        // first settlement can spend it and we know its DAA score. It must confirm; its absence is
        // a real liveness failure worth the panicking confirm. This is the one residual RPC
        // confirm - the bridge publishes settlements, not the covenant-creating bootstrap.
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
        // TODO: fee-bump a settlement that does not confirm within a deadline, rather than awaiting
        // the watch indefinitely.
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
        // in-memory covenant no longer matches this bundle's proving base, consult the settlement
        // watch the bridge writes: it carries the covenant's last settlement the bridge observed in
        // an accepted chain block, including its DAA score. When that settlement is ahead of `cov`,
        // adopt it as the new optimistic tip so a later bundle that chains from it settles instead
        // of leaving us permanently stuck behind.
        //
        // The watch is read, not the per-bundle snapshot: the settler advances `cov` optimistically
        // when it settles, but the bridge needs ≈RTT to observe that, so the watch lags `cov` by
        // one settlement. Adoption is therefore gated on the watch being *ahead*
        // (`s.new_state` differs from `cov.state`) and forward-only (`s.daa_score` at or
        // past `cov.daa_score`); a value behind `cov` (a competitor we already passed) is
        // ignored. The continuation outpoint is adopted without an on-chain confirm: the
        // bridge only publishes settlements from accepted chain blocks, so the UTXO
        // existed, and the rare case it was already spent by a reorg/race is caught at
        // settle time (the sink's `Superseded`), which skips the bundle.
        if cov.state != artifact.prev_state {
            let latest = *cfg.settlement.borrow();
            if let Some(s) = latest {
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

        // Test-only: wait our turn so competing settlers alternate rather than one sweeping the
        // ranges. Production leaves this `None` and settles as soon as a bundle is ready.
        #[cfg(feature = "test-utils")]
        if let Some((me, pacer)) = &cfg.alternation {
            pacer.await_turn(*me, &shutdown).await;
            if shutdown.is_open() {
                break;
            }
        }
        match settler.settle_one(&cov, &artifact, &shutdown).await {
            SettleOutcome::Advanced(next) => {
                cov = next;
                #[cfg(feature = "test-utils")]
                if let Some((me, pacer)) = &cfg.alternation {
                    pacer.mark_settled(*me);
                }
            }
            // A competitor's settlement is already spending this covenant outpoint, so ours can
            // never land. Hold `cov` and drop the bundle: once that settlement confirms in a chain
            // block, the bridge publishes it to the settlement watch and the reconcile block above
            // adopts it.
            SettleOutcome::Superseded => continue,
            SettleOutcome::Shutdown => break,
        }
    }
    log::info!("settlement-worker: shut down");
}
