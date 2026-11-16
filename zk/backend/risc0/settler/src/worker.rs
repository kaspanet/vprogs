//! Production settlement worker that lands aggregate-prover bundles on L1 through the wRPC-backed
//! [`Settler`](crate::settle::Settler).

mod config;

use std::time::Duration;

#[cfg(feature = "test-utils")]
pub use config::AlternationPacer;
pub use config::{SettlementMode, SettlementWorkerConfig};
use vprogs_core_atomics::{AsyncQueue, AtomicAsyncLatch};
use vprogs_l1_types::SettlementInfo;
use vprogs_state_settlement_journal::StoreJournal;
use vprogs_storage_types::Store;
use vprogs_zk_aggregate_prover::{ScheduledBundle, SettlementArtifact};
use vprogs_zk_backend_risc0_api::Receipt;

use crate::{
    confirm::{CovenantLiveness, OutpointAt, confirm_outpoint, covenant_liveness},
    covenant::{CovenantState, covenant_from_settlement},
    settle::{RpcSink, SettleOutcome, Settler, WalletFeeSource},
};

/// Backoff between retries when the funder has no spendable fee UTXO. Funding exhaustion is
/// recoverable (a later deposit to the funder lets the same bundle settle), so the worker waits
/// this long (shutdown-interruptible) and retries rather than dropping the bundle or stopping.
const FEE_EXHAUSTED_BACKOFF: Duration = Duration::from_secs(5);

/// Backoff between retries of a bundle whose superseding spend is mempool-only. The competitor's
/// transaction either confirms (the base outpoint disappears, the bundle resolves superseded) or
/// is evicted (the retry submits cleanly), so the wait is bounded by the mempool's own resolution.
const SUPERSEDED_HOLD_BACKOFF: Duration = Duration::from_secs(5);

/// How a supersede resolved: whether the bundle terminates or stays held for another attempt.
enum SupersedeResolution {
    /// Terminate the bundle. Its journal entry is deleted only when the spend is chain-confirmed;
    /// otherwise (no journal wired, shutdown mid-poll, or a confirmed settlement forking the
    /// covenant past a still-live base) the entry stays and the restart resume re-feeds the range.
    Resolved,
    /// Hold the bundle and retry it: the base outpoint is still live and the artifact still chains
    /// from the covenant, so the only racer is a mempool-only spend that may still vanish.
    Retain,
}

/// Reconciles the covenant against a bundle whose proving base may no longer match it, adopting
/// `latest` when it is ahead, and returns whether the artifact chains from the resulting position.
fn reconcile<R>(
    cov: &mut CovenantState,
    artifact: &SettlementArtifact<R>,
    latest: Option<SettlementInfo>,
) -> bool {
    let chains = |cov: &CovenantState| {
        cov.state == artifact.prev_state && cov.lane_tip == artifact.prev_lane_tip
    };
    if chains(cov) {
        return true;
    }
    if let Some(s) = latest {
        if (s.new_state != cov.state || s.new_lane_tip != cov.lane_tip)
            && s.daa_score.get() >= cov.daa_score
        {
            *cov = covenant_from_settlement(cov, &s);
            log::info!(
                "settlement-worker: adopted external settlement {} (covenant advanced to daa {})",
                s.tx_id,
                cov.daa_score,
            );
        }
    }
    chains(cov)
}

/// Resolves one superseded bundle on its skip path so the skip terminates the bundle instead of
/// re-arming it: adopts the spending settlement from a fresh settlement-watch read (the skip
/// decision's read may predate the bridge publishing the competitor's landing; once adopted, the
/// next bundle chains from the tip instead of skipping), then deletes the bundle's journal entry
/// so the aggregate prover's resume stops re-feeding the same bundle.
///
/// The delete is gated on [`covenant_liveness`], not the raw already-spent rejection: that
/// rejection also fires while the competitor's settlement only sits in the mempool, where it can
/// still be evicted or reorged, while an outpoint spent in a chain block cannot revert. A
/// mempool-only spend returns [`SupersedeResolution::Retain`] when the artifact still chains
/// from the covenant, so the caller holds the bundle until the racer resolves instead of
/// dropping it with its journal entry still present: the entry would then name a bundle nothing
/// drives, and the aggregate prover's suffix gate would skip re-driving the range forever.
/// `liveness` reads the covenant outpoint the bundle chains from, captured by the caller before
/// this call's adoption (a post-adoption `cov` names the competitor's still-unspent
/// continuation); it is injected as a closure so the resolution is testable without a node.
async fn resolve_superseded<S, R, L, Fut>(
    cov: &mut CovenantState,
    artifact: &SettlementArtifact<R>,
    bundle: &ScheduledBundle<SettlementArtifact<R>>,
    journal: Option<&StoreJournal<S>>,
    latest: Option<SettlementInfo>,
    liveness: L,
) -> SupersedeResolution
where
    S: Store,
    L: FnOnce() -> Fut,
    Fut: std::future::Future<Output = CovenantLiveness>,
{
    // Adoption first, on a fresh watch read; reconcile itself logs when it adopts.
    let chains = reconcile(cov, artifact, latest);
    match liveness().await {
        CovenantLiveness::Spent => {
            if let Some(journal) = journal {
                let start = bundle.checkpoint_index();
                journal.delete(start);
                log::info!(
                    "settlement-worker: skipping superseded bundle {start}..={}; resolved on chain",
                    start + bundle.batches() as u64 - 1,
                );
            }
            SupersedeResolution::Resolved
        }
        // The spend is mempool-only and may still vanish, so the entry stays queued.
        CovenantLiveness::Unspent if chains => SupersedeResolution::Retain,
        // A still-live base that no longer chains means a confirmed settlement forked the
        // covenant past it; the bundle cannot settle or retry, so it terminates with its entry
        // left for the restart resume. Shutdown: the loop unwinds and the next run's resume
        // re-feeds the bundle the same way.
        CovenantLiveness::Unspent | CovenantLiveness::Shutdown => SupersedeResolution::Resolved,
    }
}

/// Runs the production settlement loop until `shutdown` opens or an unrecoverable settlement
/// failure (a node hard-reject) stops it; temporary funding exhaustion is retried, not fatal.
///
/// Settlements are serialized: each queued bundle is awaited, skipped if it resolves without an
/// artifact, or settled before the next bundle is processed.
pub async fn run<S: Store>(
    queue: AsyncQueue<ScheduledBundle<SettlementArtifact<Receipt>>>,
    cfg: SettlementWorkerConfig<S>,
    covenant: CovenantState,
    shutdown: AtomicAsyncLatch,
) {
    let mut cov = Box::new(covenant);

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
    // (outpoint `tx_id:0`, SPK from the observed settlement's continuation hash) without an
    // on-chain confirm.
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
    // The advance filter is two-pin, matching the loop's reconcile: a state-neutral settlement
    // (identical root on both sides) still counts as an advance through its lane tip, while the
    // settlement `cov` already holds stays ignored, on equal pins or below the stamped score.
    let initial = *cfg.settlement.borrow();
    if let Some(s) = initial.filter(|s| {
        (s.new_state != cov.state || s.new_lane_tip != cov.lane_tip)
            && s.daa_score.get() >= cov.daa_score
    }) {
        *cov = covenant_from_settlement(&cov, &s);
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

    'outer: loop {
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

        // A competing settler may have advanced the covenant since our last settlement, so the
        // bundle's proving base may no longer match `cov` on either pin (state root, lane tip).
        // The reconcile consults the settlement watch the bridge writes: it carries the
        // covenant's last settlement the bridge observed in an accepted chain block, including
        // its DAA score. When that settlement is ahead of `cov`, adopt it as the new optimistic
        // tip so a later bundle that chains from it settles instead of leaving us permanently
        // stuck behind.
        //
        // The watch is read, not the per-bundle snapshot: the settler advances `cov` optimistically
        // when it settles, but the bridge needs ≈RTT to observe that, so the watch lags `cov` by
        // one settlement. Adoption is therefore gated on the watch being *ahead* and forward-only
        // (the settlement's `daa_score` at or past `cov.daa_score`); a value behind `cov` (a
        // competitor we already passed) is ignored. The continuation outpoint is adopted without an
        // on-chain confirm: the bridge only publishes settlements from accepted chain blocks, so
        // the UTXO existed, and the rare case it was already spent by a reorg/race is caught at
        // settle time (the sink's `Superseded`), which skips the bundle.
        //
        // A resolution holding this bundle (`Retain`) re-runs the whole block on the same bundle
        // after a backoff, so a supersede whose spend is only in the mempool neither drops the
        // bundle nor leaves its journal entry naming an undriven range: the bundle settles once
        // the competitor's transaction is evicted, or resolves on chain once it confirms.
        'bundle: loop {
            // The covenant UTXO this bundle chains from, captured before the reconcile below may
            // adopt a competitor tip: the supersede resolution verifies the spend of THIS outpoint,
            // never the adopted continuation's (which the competitor left unspent).
            let base_spk = cov.spk.clone();
            let base_outpoint = cov.outpoint;
            let latest = *cfg.settlement.borrow();
            if !reconcile(&mut cov, &artifact, latest) {
                // The base still mismatches after adopting the tip, so a competitor already covered
                // this bundle's range: it is superseded. Skip it rather than asserting in the
                // builder; a later bundle chaining from the adopted tip settles. Resolve it on the
                // way out, or the prover's resume re-feeds the same bundle on every pass with no
                // settlement ever produced.
                log::info!(
                    "settlement-worker: skipping superseded bundle (a competitor covered its range)"
                );
                let latest = *cfg.settlement.borrow();
                let resolution = resolve_superseded(
                    &mut cov,
                    &artifact,
                    &bundle,
                    cfg.journal.as_ref(),
                    latest,
                    || {
                        covenant_liveness(
                            &cfg.client,
                            &cfg.params,
                            OutpointAt { spk: &base_spk, outpoint: base_outpoint },
                            &shutdown,
                        )
                    },
                )
                .await;
                match resolution {
                    SupersedeResolution::Resolved => continue 'outer,
                    SupersedeResolution::Retain => {
                        tokio::select! {
                            biased;
                            () = shutdown.wait() => break 'outer,
                            () = tokio::time::sleep(SUPERSEDED_HOLD_BACKOFF) => {}
                        }
                        continue 'bundle;
                    }
                }
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
            // Settle this bundle, retrying the same one while funding is temporarily exhausted: an
            // empty fee set is recoverable (a later deposit to the funder lets it settle), so
            // back off and retry rather than dropping the bundle. A node hard-reject is not
            // recoverable, so stop.
            let advanced = loop {
                match settler.settle_one(&cov, &artifact, &shutdown).await {
                    SettleOutcome::Advanced(next) => break next,
                    // A competitor's settlement is already spending this covenant outpoint, so ours
                    // can never land. Resolve the bundle on the way out: the outpoint settle just
                    // failed to spend is the one whose chain spend retires the range. A
                    // mempool-only spend retains the bundle for another attempt after a backoff:
                    // the racer either confirms (the base disappears, the bundle resolves on
                    // chain) or is evicted (the retry submits cleanly).
                    SettleOutcome::Superseded => {
                        let spent_spk = cov.spk.clone();
                        let spent_outpoint = cov.outpoint;
                        let latest = *cfg.settlement.borrow();
                        let resolution = resolve_superseded(
                            &mut cov,
                            &artifact,
                            &bundle,
                            cfg.journal.as_ref(),
                            latest,
                            || {
                                covenant_liveness(
                                    &cfg.client,
                                    &cfg.params,
                                    OutpointAt { spk: &spent_spk, outpoint: spent_outpoint },
                                    &shutdown,
                                )
                            },
                        )
                        .await;
                        match resolution {
                            SupersedeResolution::Resolved => continue 'outer,
                            SupersedeResolution::Retain => {
                                tokio::select! {
                                    biased;
                                    () = shutdown.wait() => break 'outer,
                                    () = tokio::time::sleep(SUPERSEDED_HOLD_BACKOFF) => {}
                                }
                                continue;
                            }
                        }
                    }
                    SettleOutcome::Shutdown => break 'outer,
                    // The node hard-rejected the submission; retrying the same bundle cannot
                    // recover it. Stop the settler with a logged reason rather
                    // than panicking the worker task.
                    SettleOutcome::Failed(reason) => {
                        log::error!(
                            "settlement-worker: unrecoverable settlement failure: {reason}; stopping"
                        );
                        break 'outer;
                    }
                    // Funding is exhausted right now but may be replenished; log and retry the same
                    // bundle after a backoff, staying interruptible so teardown is not held up.
                    SettleOutcome::FeeExhausted => {
                        log::warn!(
                            "settlement-worker: no spendable fee UTXO; backing off {}s and retrying \
                         (awaiting a deposit to the funder)",
                            FEE_EXHAUSTED_BACKOFF.as_secs(),
                        );
                        tokio::select! {
                            biased;
                            () = shutdown.wait() => break 'outer,
                            () = tokio::time::sleep(FEE_EXHAUSTED_BACKOFF) => {}
                        }
                    }
                }
            };
            cov = advanced;
            #[cfg(feature = "test-utils")]
            if let Some((me, pacer)) = &cfg.alternation {
                pacer.mark_settled(*me);
            }
            break 'bundle;
        }
    }
    log::info!("settlement-worker: shut down");
}

#[cfg(test)]
mod tests {
    use kaspa_consensus_core::tx::TransactionOutpoint;
    use kaspa_hashes::Hash;
    use vprogs_l1_types::{SettlementInfo, TransactionId};
    use vprogs_state_settlement_journal::{JournalEntry, StoreJournal};
    use vprogs_storage_rocksdb_store::RocksDbStore;
    use vprogs_zk_aggregate_prover::{BundleBlocks, ScheduledBundle};

    use super::{
        CovenantState, SettlementArtifact, SupersedeResolution, reconcile, resolve_superseded,
    };
    use crate::confirm::CovenantLiveness;

    /// State root every state-neutral settlement leaves unchanged: a lane range of guest-rejected
    /// transactions writes no resource, so the root is identical on both sides of it.
    const STATE: [u8; 32] = [0x11; 32];

    /// A different state root, for pins that genuinely advanced the state.
    const OTHER_STATE: [u8; 32] = [0x22; 32];

    /// Covenant holding `(state, lane_tip)`, the pair its redeem prefix pins.
    fn covenant(state: [u8; 32], lane_tip: u8) -> CovenantState {
        CovenantState {
            covenant_id: Hash::from_bytes([0xCC; 32]),
            state,
            lane_tip: Hash::from_bytes([lane_tip; 32]),
            outpoint: TransactionOutpoint::new(Hash::from_bytes([0x77; 32]), 0),
            spk: Default::default(),
            value: 0,
            daa_score: 0,
        }
    }

    /// Bundle artifact chaining from `(prev_state, prev_lane_tip)`. The receipt never plays a role
    /// in the reconcile, so the generic parameter is the unit type.
    fn artifact(prev_state: [u8; 32], prev_lane_tip: u8) -> SettlementArtifact<()> {
        SettlementArtifact {
            receipt: (),
            block_prove_to: Hash::from_bytes([0x02; 32]),
            prev_state,
            prev_lane_tip: Hash::from_bytes([prev_lane_tip; 32]),
            new_state: OTHER_STATE,
            new_lane_tip: Hash::from_bytes([0x60; 32]),
            new_seq_commit: Hash::default(),
            permission_spk_hash: [0u8; 32],
            deposit_spk_hash: [0u8; 32],
            covenant_id: [0xCC; 32],
        }
    }

    /// The bridge-observed settlement advancing the covenant to `(new_state, new_lane_tip)`.
    fn settlement(new_state: [u8; 32], new_lane_tip: u8) -> SettlementInfo {
        SettlementInfo {
            tx_id: TransactionId::from([0x99; 32]),
            new_state,
            new_lane_tip: Hash::from_bytes([new_lane_tip; 32]),
            ..Default::default()
        }
    }

    /// A state-neutral duplicate re-proved over an already-settled range chains its `prev_state`
    /// from the flat root (the guard's state key alone cannot distinguish it) but its
    /// `prev_lane_tip` from a tip the covenant already advanced past. The reconcile must reject
    /// it: reaching the builder with a stale tip panics the settler task.
    #[test]
    fn state_neutral_duplicate_with_a_stale_lane_tip_is_superseded() {
        let mut cov = covenant(STATE, 2);
        let duplicate = artifact(STATE, 1);

        assert!(!reconcile(&mut cov, &duplicate, None), "no watch to adopt from");

        // The watch already carries the covenant's own position, the state-neutral settlement
        // that advanced the tip: there is nothing newer to adopt, so the duplicate stays
        // superseded.
        let watch = settlement(STATE, 2);
        assert!(!reconcile(&mut cov, &duplicate, Some(watch)));
        assert_eq!(cov.lane_tip, Hash::from_bytes([2; 32]), "nothing was adopted");
    }

    /// A bundle chaining from a state-neutral external settlement advances only the lane tip, so
    /// the watch settlement ahead of us matches our state root while differing in its tip. The
    /// reconcile must adopt it, or every later bundle silently fails the lane-tip chain forever.
    #[test]
    fn state_neutral_external_advance_is_adopted() {
        let mut cov = covenant(STATE, 1);
        let next = artifact(STATE, 2);
        let watch = settlement(STATE, 2);

        assert!(reconcile(&mut cov, &next, Some(watch)));
        assert_eq!(cov.lane_tip, Hash::from_bytes([2; 32]), "the watch tip was adopted");
        assert_eq!(cov.state, STATE, "the state was carried over");
    }

    /// A bundle chaining from the covenant exactly settles without touching the watch.
    #[test]
    fn chaining_artifact_settles_without_adoption() {
        let mut cov = covenant(STATE, 1);
        let art = artifact(STATE, 1);

        assert!(reconcile(&mut cov, &art, None));
        assert_eq!(cov.lane_tip, Hash::from_bytes([1; 32]), "nothing was adopted");
    }

    /// A bundle whose state root the covenant already advanced past stays superseded, and a watch
    /// holding the covenant's own position changes nothing: the next bundle chains from the
    /// adopted tip instead.
    #[test]
    fn state_advancing_competitor_supersedes_without_readopting() {
        let mut cov = covenant(OTHER_STATE, 3);
        let stale = artifact(STATE, 1);
        let watch = settlement(OTHER_STATE, 3);

        assert!(!reconcile(&mut cov, &stale, Some(watch)));
        assert_eq!(cov.state, OTHER_STATE);
        assert_eq!(cov.lane_tip, Hash::from_bytes([3; 32]), "nothing was readopted");
    }

    /// Journal entry matching [`scheduled`]: the bundle `7..=8` the resume path would re-feed.
    fn journaled_entry() -> JournalEntry {
        JournalEntry {
            end_index: 8,
            from_block: Hash::from_bytes([0x01; 32]),
            block_prove_to: Hash::from_bytes([0x02; 32]),
            seq_commit: Hash::from_bytes([0x03; 32]),
        }
    }

    /// A journal over a throwaway store holding the scheduled bundle's entry, standing in for the
    /// prover's persisted journal.
    fn recorded_journal() -> (tempfile::TempDir, StoreJournal<RocksDbStore>) {
        let dir = tempfile::TempDir::new().unwrap();
        let journal = StoreJournal::new(RocksDbStore::open(dir.path()));
        journal.record(7, &journaled_entry());
        (dir, journal)
    }

    /// A two-batch bundle whose first checkpoint is 7 (`7..=8`): the start key the resolution
    /// must delete, matching the resume path's re-feed key.
    fn scheduled() -> ScheduledBundle<SettlementArtifact<()>> {
        ScheduledBundle::new(
            2,
            7,
            BundleBlocks {
                from_block: Hash::from_bytes([0x01; 32]),
                block_prove_to: Hash::from_bytes([0x02; 32]),
            },
        )
    }

    /// The chain-confirmed supersede: the covenant outpoint gone from the UTXO set means the
    /// competitor's settlement is in a block, so the bundle terminates and its journal entry dies
    /// at its checkpoint index, stopping the resume path's re-feed of the range.
    #[tokio::test]
    async fn chain_spent_supersede_deletes_the_journal_entry() {
        let (_dir, journal) = recorded_journal();
        let mut cov = covenant(STATE, 1);
        let bundle = scheduled();

        let resolution = resolve_superseded(
            &mut cov,
            &artifact(STATE, 2),
            &bundle,
            Some(&journal),
            None,
            || async { CovenantLiveness::Spent },
        )
        .await;

        assert!(matches!(resolution, SupersedeResolution::Resolved));
        assert!(journal.entries().is_empty(), "entry deleted at the bundle's checkpoint index");
    }

    /// A supersede whose spend is mempool-only (the covenant outpoint still unspent on chain, the
    /// artifact still chaining from the covenant) retains the bundle with its journal entry in
    /// place: the competitor's settlement can be evicted or reorged, and an entry naming a bundle
    /// the settler no longer holds would strand the aggregate prover's suffix gate.
    #[tokio::test]
    async fn mempool_only_supersede_retains_the_bundle_for_retry() {
        let (_dir, journal) = recorded_journal();
        let mut cov = covenant(STATE, 2);
        let bundle = scheduled();

        let resolution = resolve_superseded(
            &mut cov,
            &artifact(STATE, 2),
            &bundle,
            Some(&journal),
            None,
            || async { CovenantLiveness::Unspent },
        )
        .await;

        assert!(matches!(resolution, SupersedeResolution::Retain));
        assert_eq!(journal.entries().len(), 1, "mempool spender may vanish");
    }

    /// A still-live base that no longer chains (a confirmed settlement forked the covenant past
    /// it, adopted by the resolution's own reconcile) terminates the bundle without deleting its
    /// entry: the range is neither pending nor chain-resolved, so the restart resume re-feeds it.
    #[tokio::test]
    async fn live_base_past_a_forked_tip_terminates_without_delete() {
        let (_dir, journal) = recorded_journal();
        let mut cov = covenant(STATE, 1);
        let bundle = scheduled();

        let resolution = resolve_superseded(
            &mut cov,
            &artifact(STATE, 2),
            &bundle,
            Some(&journal),
            // A fork settlement adopting a tip the artifact does not chain from.
            Some(settlement(OTHER_STATE, 3)),
            || async { CovenantLiveness::Unspent },
        )
        .await;

        assert!(matches!(resolution, SupersedeResolution::Resolved));
        assert_eq!(journal.entries().len(), 1, "an unspent fork keeps the entry for the resume");
    }

    /// Shutdown mid-liveness terminates the bundle with its entry left for the next run's resume.
    #[tokio::test]
    async fn shutdown_during_liveness_terminates_without_delete() {
        let (_dir, journal) = recorded_journal();
        let mut cov = covenant(STATE, 1);
        let bundle = scheduled();

        let resolution = resolve_superseded(
            &mut cov,
            &artifact(STATE, 2),
            &bundle,
            Some(&journal),
            None,
            || async { CovenantLiveness::Shutdown },
        )
        .await;

        assert!(matches!(resolution, SupersedeResolution::Resolved));
        assert_eq!(journal.entries().len(), 1, "entry kept for the restart resume");
    }

    /// The resolution adopts a watch settlement ahead of `cov` even with no journal wired, so the
    /// next bundle chains from the competitor's tip instead of skipping against it.
    #[tokio::test]
    async fn supersede_resolution_adopts_the_spending_tip() {
        let mut cov = covenant(STATE, 1);
        let bundle = scheduled();

        resolve_superseded(
            &mut cov,
            &artifact(STATE, 2),
            &bundle,
            None::<&StoreJournal<RocksDbStore>>,
            Some(settlement(STATE, 2)),
            || async { CovenantLiveness::Spent },
        )
        .await;

        assert_eq!(cov.lane_tip, Hash::from_bytes([2; 32]), "watch tip adopted on the skip");
        assert_eq!(cov.state, STATE, "state carried over");
    }
}
