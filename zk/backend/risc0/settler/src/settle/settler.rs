//! Environment-agnostic settlement execution for proven bundles. The production daemon and L2 sim
//! drive the same [`Settler`] with their own [`FeeSource`] and [`SettlementSink`] implementations.

use std::collections::HashSet;

use tokio::sync::watch;
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_l1_types::SettlementInfo;
use vprogs_zk_aggregate_prover::SettlementArtifact;
use vprogs_zk_backend_risc0_api::{Backend, Receipt};

use crate::{
    SettlementMode,
    confirm::OutpointAt,
    covenant::{CovenantState, build_settlement_for_mode},
    settle::effects::{FeeSource, SettlementSink, SubmitOutcome},
};

/// The result of attempting to settle one bundle.
pub enum SettleOutcome {
    /// The settlement landed and confirmed; the covenant advanced to this state.
    Advanced(Box<CovenantState>),
    /// A competitor already spent this covenant outpoint, so this bundle is superseded. The caller
    /// holds its covenant and waits to adopt the competitor's settlement once it confirms.
    Superseded,
    /// `shutdown` opened mid-settlement; the caller should stop.
    Shutdown,
    /// No spendable fee UTXO was available: every candidate was rejected. This is recoverable
    /// (fresh funds may later arrive at the funder), so the caller backs off and retries the same
    /// bundle rather than dropping it or stopping.
    FeeExhausted,
    /// The node hard-rejected the submission; retrying the same bundle cannot recover it. Carries a
    /// human-readable reason. The caller stops the settler rather than looping on a doomed
    /// submission.
    Failed(String),
}

/// Settles proven bundles against one covenant using injected funding and submission effects.
pub struct Settler<F: FeeSource, K: SettlementSink> {
    /// Funds and signs each settlement's fee.
    funder: F,
    /// Submits each funded settlement and reports the network's verdict.
    sink: K,
    /// Backend, for the covenant's redeem pins when building the settlement.
    backend: Backend,
    /// Lane key the covenant SPK pins.
    lane_key: kaspa_hashes::Hash,
    /// Which redeem variant to settle against.
    mode: SettlementMode,
    /// The covenant's last on-chain settlement, fed by the chain observer. `settle_one` awaits a
    /// change past the bundle's base as its confirmation signal.
    settlement: watch::Receiver<Option<SettlementInfo>>,
}

impl<F: FeeSource, K: SettlementSink> Settler<F, K> {
    /// Assembles a settler from its injected funder and sink, the build context (backend, lane key,
    /// redeem mode), and a receiver on the settlement watch the chain observer feeds.
    pub fn new(
        funder: F,
        sink: K,
        backend: Backend,
        lane_key: kaspa_hashes::Hash,
        mode: SettlementMode,
        settlement: watch::Receiver<Option<SettlementInfo>>,
    ) -> Self {
        Self { funder, sink, backend, lane_key, mode, settlement }
    }

    /// Settles one proven bundle and returns whether it advanced the covenant, was superseded, hit
    /// recoverable fee exhaustion (retry the same bundle), failed unrecoverably, or shut down.
    pub async fn settle_one(
        &self,
        cov: &CovenantState,
        artifact: &SettlementArtifact<Receipt>,
        shutdown: &AtomicAsyncLatch,
    ) -> SettleOutcome {
        // Build the settlement and its covenant compute budget from the bundle's authoritative
        // bounds. Asserts the live covenant agrees before we pay to submit.
        let built =
            build_settlement_for_mode(self.mode, &self.backend, &self.lane_key, cov, artifact);
        let covenant_entry = cov.utxo_entry();
        let covenant = OutpointAt { spk: &cov.spk, outpoint: cov.outpoint };

        // The network can reject a settlement for a transient reason that funding the fee from a
        // different UTXO resolves. Re-fund from another settled UTXO, excluding each rejected one,
        // until one is accepted or every spendable UTXO is exhausted.
        let mut excluded = HashSet::new();
        let txid = loop {
            // Re-funding from another UTXO each rejection is the one unbounded wait here; bail on
            // shutdown so teardown is not held up retrying a doomed submission to exhaustion.
            if shutdown.is_open() {
                return SettleOutcome::Shutdown;
            }
            let Some(funded) = self.funder.fund(&built, covenant_entry.clone(), &excluded).await
            else {
                return SettleOutcome::FeeExhausted;
            };
            match self.sink.submit(&funded.tx, covenant, shutdown).await {
                SubmitOutcome::Accepted(txid) => break txid,
                SubmitOutcome::FeeRejected => {
                    log::warn!(
                        "settlement-worker: fee UTXO {} rejected, retrying with another UTXO",
                        funded.fee_outpoint,
                    );
                    excluded.insert(funded.fee_outpoint);
                }
                SubmitOutcome::Superseded => {
                    log::info!(
                        "settlement-worker: covenant outpoint {} already spent by a competitor; \
                         skipping superseded bundle",
                        cov.outpoint,
                    );
                    return SettleOutcome::Superseded;
                }
                SubmitOutcome::Fatal(reason) => {
                    return SettleOutcome::Failed(format!(
                        "node rejected the settlement: {reason}"
                    ));
                }
                SubmitOutcome::Shutdown => return SettleOutcome::Shutdown,
            }
        };
        log::info!(
            "settlement-worker: submitted settlement {txid} (block {})",
            artifact.block_prove_to,
        );

        // Confirm by awaiting the settlement watch rather than polling: the chain observer
        // publishes the covenant's last settlement, so a change past `cov` is exactly the
        // confirmation signal. The predicate gates on the published settlement advancing
        // the state (`new_state` differs) and being forward-only (`daa_score` at or past
        // `cov`), so a stale handle one settlement behind never matches. `wait_for` returns
        // immediately if the current value already satisfies it (our settlement may have
        // landed before we started awaiting).
        let target_state = cov.state;
        let min_daa = cov.daa_score;
        let mut rx = self.settlement.clone();
        let matched = tokio::select! {
            biased;
            () = shutdown.wait() => return SettleOutcome::Shutdown,
            res = rx.wait_for(|opt| {
                opt.is_some_and(|s| s.new_state != target_state && s.daa_score.get() >= min_daa)
            }) => res,
        };
        // Copy the settlement out of the borrow and drop the `Ref` promptly so the sender is not
        // blocked. A dropped sender (`Err`) is the shutdown signal.
        let info = match matched {
            Ok(r) => (*r).expect("wait_for predicate matched a Some"),
            Err(_) => return SettleOutcome::Shutdown,
        };

        if info.tx_id == txid {
            log::info!(
                "settlement-worker: settlement {txid} confirmed (daa {})",
                info.daa_score.get(),
            );
            SettleOutcome::Advanced(Box::new(built.advance.apply(txid, info.daa_score.get())))
        } else {
            // A competitor's settlement advanced the covenant before ours confirmed (a reorg
            // replaced ours, or it never made the chain). Hold our covenant and let the caller's
            // mid-loop reconcile adopt the competitor's tip from the same watch.
            log::info!(
                "settlement-worker: covenant advanced by competitor {} while confirming {txid}; \
                 superseding this bundle",
                info.tx_id,
            );
            SettleOutcome::Superseded
        }
    }
}
