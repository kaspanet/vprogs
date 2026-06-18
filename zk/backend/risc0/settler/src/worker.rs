use std::{
    collections::HashSet,
    ops::Range,
    sync::{Arc, Mutex},
    time::Duration,
};

use kaspa_addresses::Prefix;
use kaspa_consensus_core::{
    config::params::Params,
    tx::{ScriptPublicKey, TransactionOutpoint, UtxoEntry},
};
use kaspa_hashes::Hash;
use kaspa_rpc_core::{RpcError, api::rpc::RpcApi};
use kaspa_txscript::standard::extract_script_pub_key_address;
use kaspa_wrpc_client::prelude::KaspaRpcClient;
use secp256k1::Keypair;
use vprogs_core_atomics::{AsyncQueue, AtomicAsyncLatch};
use vprogs_l1_wallet::Wallet;
use vprogs_zk_aggregate_prover::{ScheduledBundle, SettlementArtifact};
use vprogs_zk_backend_risc0_api::{Backend, Receipt};

use crate::covenant::{
    CovenantState, build_dev_settlement, build_settlement, covenant_from_settlement,
};

/// Poll cadence and ceiling for waiting on a covenant UTXO to confirm on chain.
const CONFIRM_POLL_INTERVAL: Duration = Duration::from_secs(1);
const CONFIRM_MAX_POLLS: u32 = 300;

/// Short ceiling for confirming an adopted competitor settlement's continuation UTXO. The
/// settlement is already on chain when it is the live tip, so its UTXO is found within a poll or
/// two; a backward snapshot reconstructs an already-spent (and so never-unspent-again) outpoint
/// that never appears. A short ceiling distinguishes the two without the long liveness poll a
/// settlement we submitted ourselves warrants.
const ADOPT_MAX_POLLS: u32 = 5;

/// Which redeem variant the worker settles against. The caller picks it; the operating contract is
/// to settle in [`Production`](SettlementMode::Production) only when real (CUDA) proofs are in play
/// and in [`Dev`](SettlementMode::Dev) under `RISC0_DEV_MODE`, where the prover emits stub receipts
/// the production `OpZkPrecompile` would reject.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SettlementMode {
    /// Production redeem: the on-chain `OpZkPrecompile` verifies the bundle's real receipt.
    Production,
    /// Dev redeem: the chain anchors the claimed seq commit; no proof is verified on chain.
    Dev,
}

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
    /// Backend, for the covenant's redeem pins (guest image ids). Unused in
    /// [`SettlementMode::Dev`] (the dev redeem pins no image ids).
    pub backend: Backend,
    /// Whether to settle against the production or dev redeem variant.
    pub mode: SettlementMode,
    /// Optional millisecond window to jitter each submission by. `None` submits immediately (the
    /// production default). Multiple provers settling one covenant race to spend the same
    /// outpoint; without jitter the same prover's submission deterministically wins every
    /// range. A small random pre-submit delay models real relay-timing variance so the winner
    /// alternates.
    pub submit_jitter: Option<Range<u64>>,
    /// Test-only alternation: `(this settler's id, pacer shared with the competitor)`. When set, a
    /// settler that landed the previous settlement waits until a different settler lands one
    /// before settling again, so competing provers strictly alternate instead of one sweeping
    /// every range (and each settles at half rate, so its recycled fee-change UTXO confirms
    /// before reuse). `None` in production, where settlers race freely.
    pub alternation: Option<(u8, Arc<AlternationPacer>)>,
}

/// Forces two competing settlers to alternate, used only by the contention test. Holds the id of
/// whoever settled last; a settler that finds itself there waits on `bell` until the other reports.
/// A short poll fallback re-checks the turn so a missed notification can never wedge the wait.
pub struct AlternationPacer {
    last: Mutex<Option<u8>>,
    bell: tokio::sync::Notify,
}

impl AlternationPacer {
    pub fn new() -> Self {
        Self { last: Mutex::new(None), bell: tokio::sync::Notify::new() }
    }

    /// Blocks until it is not `me`'s turn to defer (a different settler reported since `me`, or
    /// none has yet). Returns early when `shutdown` opens so teardown is not held up.
    async fn await_turn(&self, me: u8, shutdown: &AtomicAsyncLatch) {
        while *self.last.lock().unwrap() == Some(me) {
            tokio::select! {
                biased;
                () = shutdown.wait() => return,
                () = self.bell.notified() => {}
                () = tokio::time::sleep(Duration::from_millis(25)) => {}
            }
        }
    }

    /// Records that `me` just settled and wakes a settler waiting for its turn.
    fn mark_settled(&self, me: u8) {
        *self.last.lock().unwrap() = Some(me);
        self.bell.notify_waiters();
    }
}

impl Default for AlternationPacer {
    fn default() -> Self {
        Self::new()
    }
}

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
        if let Some((me, pacer)) = &cfg.alternation {
            pacer.await_turn(*me, &shutdown).await;
            if shutdown.is_open() {
                break;
            }
        }
        match settle_one(&cfg, &cov, &artifact, &shutdown).await {
            SettleOutcome::Advanced(next) => {
                cov = next;
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

/// The result of attempting to settle one bundle.
#[allow(clippy::large_enum_variant)]
enum SettleOutcome {
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
async fn settle_one(
    cfg: &SettlementWorkerConfig,
    cov: &CovenantState,
    artifact: &SettlementArtifact<Receipt>,
    shutdown: &AtomicAsyncLatch,
) -> SettleOutcome {
    // Build the settlement and its covenant compute budget from the bundle's authoritative bounds
    // (shared with the sim driver). Asserts the live covenant agrees before we pay to submit.
    let built = match cfg.mode {
        SettlementMode::Production => build_settlement(&cfg.backend, &cfg.lane_key, cov, artifact),
        SettlementMode::Dev => build_dev_settlement(&cfg.lane_key, cov, artifact),
    };

    let covenant_entry =
        UtxoEntry::new(cov.value, cov.spk.clone(), cov.daa_score, false, Some(cov.covenant_id));
    let wallet = Wallet::new(&cfg.client, &cfg.params, cfg.keypair);

    // The node can reject a settlement for a transient reason that funding the fee from a
    // different UTXO resolves (see [`is_retriable_fee_rejection`]). Re-fund from another settled
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

/// Whether the covenant (state) outpoint is still spendable, the disambiguator for an orphan
/// rejection (which names no input).
enum CovenantLiveness {
    /// Still an unspent UTXO: the orphan is a transient fee-UTXO problem, retry with another.
    Unspent,
    /// Gone from the UTXO set: a competitor confirmed-spent it, so the bundle is superseded.
    Spent,
    /// `shutdown` opened while polling; abandon the settlement.
    Shutdown,
}

/// Polls whether the covenant (state) outpoint is still an unspent UTXO on chain, to disambiguate
/// an orphan rejection: a settlement orphans on a missing input, and the covenant outpoint goes
/// missing exactly when a competitor confirmed-spent it (a superseded bundle), whereas a transient
/// fee orphan leaves the covenant live (a fee UTXO to swap). A single poll suffices: the covenant
/// was confirmed before we built against it, so its absence now is a spend, not a confirmation lag.
async fn covenant_liveness(
    client: &KaspaRpcClient,
    params: &Params,
    cov: &CovenantState,
    shutdown: &AtomicAsyncLatch,
) -> CovenantLiveness {
    let target = OutpointAt { spk: &cov.spk, outpoint: cov.outpoint };
    match poll_outpoint(client, params, target, shutdown, 1).await {
        ConfirmOutcome::Confirmed(_) => CovenantLiveness::Unspent,
        ConfirmOutcome::Timeout => CovenantLiveness::Spent,
        ConfirmOutcome::Shutdown => CovenantLiveness::Shutdown,
    }
}

/// The result of polling for a covenant UTXO to confirm on chain.
enum ConfirmOutcome {
    /// The outpoint appeared as an unspent UTXO; carries its block DAA score.
    Confirmed(u64),
    /// `shutdown` opened while polling.
    Shutdown,
    /// The outpoint did not appear within the poll ceiling.
    Timeout,
}

/// A specific outpoint at a covenant's P2SH SPK. Covenant UTXOs are P2SH, so the node's utxoindex
/// tracks them by their script address; confirming one means finding `outpoint` among the unspent
/// UTXOs the node reports for `spk`'s address.
#[derive(Clone, Copy)]
struct OutpointAt<'a> {
    /// The covenant UTXO's P2SH SPK, whose address the utxoindex is queried by.
    spk: &'a ScriptPublicKey,
    /// The outpoint being awaited at that SPK.
    outpoint: TransactionOutpoint,
}

/// Polls the node up to `max_polls` times for `target`'s outpoint at its P2SH address, returning
/// its block DAA score on success. Resolves to [`Shutdown`](ConfirmOutcome::Shutdown) if `shutdown`
/// opens mid-poll, or [`Timeout`](ConfirmOutcome::Timeout) if the outpoint never appears.
async fn poll_outpoint(
    client: &KaspaRpcClient,
    params: &Params,
    target: OutpointAt<'_>,
    shutdown: &AtomicAsyncLatch,
    max_polls: u32,
) -> ConfirmOutcome {
    let prefix = Prefix::from(params.net.network_type());
    let address =
        extract_script_pub_key_address(target.spk, prefix).expect("covenant P2SH address");
    for _ in 0..max_polls {
        if shutdown.is_open() {
            return ConfirmOutcome::Shutdown;
        }
        let utxos = client
            .get_utxos_by_addresses(vec![address.clone()])
            .await
            .expect("get_utxos_by_addresses");
        if let Some(entry) =
            utxos.into_iter().find(|e| TransactionOutpoint::from(e.outpoint) == target.outpoint)
        {
            return ConfirmOutcome::Confirmed(entry.utxo_entry.block_daa_score);
        }
        // Cancelable poll delay: wake on shutdown instead of sleeping out the full interval.
        tokio::select! {
            biased;
            () = shutdown.wait() => return ConfirmOutcome::Shutdown,
            () = tokio::time::sleep(CONFIRM_POLL_INTERVAL) => {}
        }
    }
    ConfirmOutcome::Timeout
}

/// Polls the node until `target`'s outpoint appears at its P2SH address, returning its block DAA
/// score. Returns `None` if `shutdown` opens while polling. Panics on timeout: a UTXO this worker
/// bootstrapped or settled itself must confirm, so its absence is a liveness failure worth
/// surfacing. The adoption path uses [`poll_outpoint`] directly instead, where a non-confirming
/// outpoint is competitor-derived (a stale snapshot) and recovered by skipping, not a panic.
async fn confirm_outpoint(
    client: &KaspaRpcClient,
    params: &Params,
    target: OutpointAt<'_>,
    shutdown: &AtomicAsyncLatch,
) -> Option<u64> {
    match poll_outpoint(client, params, target, shutdown, CONFIRM_MAX_POLLS).await {
        ConfirmOutcome::Confirmed(daa_score) => Some(daa_score),
        ConfirmOutcome::Shutdown => None,
        ConfirmOutcome::Timeout => {
            panic!("covenant outpoint {} not confirmed within timeout", target.outpoint)
        }
    }
}
