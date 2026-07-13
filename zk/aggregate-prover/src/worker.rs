use std::{
    collections::VecDeque,
    ops::RangeInclusive,
    thread::{JoinHandle, spawn},
};

use kaspa_hashes::Hash;
use tokio::{runtime::Builder, sync::watch};
use vprogs_core_atomics::AsyncQueue;
use vprogs_core_codec::Reader;
use vprogs_l1_types::{ChainBlockMetadata, SettlementInfo};
use vprogs_scheduling_scheduler::{Processor, ScheduledBatch};
use vprogs_storage_types::Store;
use vprogs_zk_abi::batch_aggregator::{Inputs as AggregatorInputs, StateTransition};
use vprogs_zk_batch_prover::{LaneProofRequest, LaneProofSource};

use crate::{
    AggregateProver, AggregateProverConfig, Backend, BundleBlocks, ScheduledBundle,
    SettlementArtifact, command::Command,
};

/// Background worker that accumulates scheduled batches, forms bundles from the consecutively-ready
/// prefix of their per-batch receipts, and proves one settlement-level receipt per bundle.
pub(crate) struct Worker<S: Store, P: Processor<S>, B: Backend, L: LaneProofSource> {
    /// Shared prover state (inbox, shutdown).
    prover: AggregateProver<S, P>,
    /// Backend used for aggregator proving.
    backend: B,
    /// Lane key this prover settles.
    lane_key: Hash,
    /// Covenant id the bundle journal is checked against, or `None` to skip the check.
    covenant_id: Option<Hash>,
    /// Source of each bundle's final-block lane proof.
    lane_source: L,
    /// Queue each formed bundle's [`ScheduledBundle`] handle is published onto for on-chain
    /// settlement, or `None` to run without settling.
    settlement_queue: Option<AsyncQueue<ScheduledBundle<SettlementArtifact<B::Receipt>>>>,
    /// Inclusive bound on how many batches one bundle may consume (min ready before forming, max
    /// per bundle).
    bundle_size: RangeInclusive<usize>,
    /// Batches accumulated but not yet bundled, in scheduling order.
    queued: VecDeque<ScheduledBatch<S, P>>,
    /// Batches consumed into a proved bundle but not yet known to be settled, in scheduling order.
    /// [`reaggregate_superseded`](Self::reaggregate_superseded) re-forms the suffix of these a
    /// shorter competitor superseded; drained once a settlement covers them.
    retained: VecDeque<ScheduledBatch<S, P>>,
    /// Receiver on the bridge's covenant `last_settlement` watch driving
    /// [`reaggregate_superseded`](Self::reaggregate_superseded), or `None` to run without
    /// re-forming.
    settlement: Option<watch::Receiver<Option<SettlementInfo>>>,
    /// First-batch checkpoint index of the most recently re-formed suffix, guarding against
    /// re-emitting it on every settlement wake. Reset by a rollback.
    last_reformed_from: Option<u64>,
}

impl<S, P, B, L> Worker<S, P, B, L>
where
    S: Store,
    B: Backend,
    L: LaneProofSource,
    P: Processor<
            S,
            TransactionArtifact = B::Receipt,
            BatchArtifact = B::Receipt,
            AggregatorArtifact = B::Receipt,
            BatchMetadata = ChainBlockMetadata,
        >,
{
    /// Spawns the worker on a new thread with a single-threaded tokio runtime and returns its join
    /// handle. The prover joins this on shutdown so the worker's GPU prover is torn down (its risc0
    /// CUDA context released) before the process exits.
    pub(crate) fn spawn(
        prover: AggregateProver<S, P>,
        backend: B,
        config: AggregateProverConfig<L, B::Receipt>,
    ) -> JoinHandle<()> {
        let AggregateProverConfig {
            lane_key,
            covenant_id,
            lane_source,
            settlement_queue,
            settlement,
            bundle_size,
        } = config;
        // Bundle formation parks while `take` is below the range start and caps `take` at the range
        // end, so an empty range (start > end) would never form a bundle. Reject it up front rather
        // than stall silently.
        assert!(
            !bundle_size.is_empty(),
            "bundle_size must be a non-empty range (start <= end); got {bundle_size:?}",
        );
        let this = Self {
            prover,
            backend,
            lane_key,
            covenant_id,
            lane_source,
            settlement_queue,
            bundle_size,
            queued: VecDeque::new(),
            retained: VecDeque::new(),
            settlement,
            last_reformed_from: None,
        };
        let runtime = Builder::new_current_thread().enable_all().build().expect("runtime");
        spawn(move || runtime.block_on(this.run()))
    }

    /// Main loop: drain commands into local state, prove every ready bundle in arrival order, and
    /// re-aggregate a superseded suffix whenever the settlement watch advances.
    async fn run(mut self) {
        loop {
            // Draining only accumulates: a bundle spans many batches and depends on which receipts
            // are ready, so bundle formation happens after the drain, not per command.
            while let Some(cmd) = self.prover.inbox.pop() {
                match cmd {
                    Command::Batch(batch) => self.queued.push_back(batch),
                    Command::Rollback(target) => self.apply_rollback(target),
                }
                if self.prover.shutdown.is_open() {
                    return;
                }
            }

            // Re-aggregate a superseded suffix when the settlement watch advances. `has_changed`
            // errors only once the bridge dropped the sender (node teardown), not on a fresh
            // settlement; `borrow_and_update` clears the flag so each settlement is acted on once.
            let changed =
                self.settlement.as_ref().is_some_and(|rx| rx.has_changed().unwrap_or(false));
            if changed {
                let latest = *self.settlement.as_mut().expect("settlement").borrow_and_update();
                self.reaggregate_superseded(latest).await;
                if self.prover.shutdown.is_open() {
                    return;
                }
            }

            // Try to prove one bundle. Loop without parking while progress is made so back-to-back
            // ready bundles drain promptly.
            let made_progress = self.try_prove_one_bundle().await;
            if self.prover.shutdown.is_open() {
                return;
            }
            if made_progress {
                continue;
            }

            // Nothing ready: park until a new command arrives, a queued batch behind the front
            // publishes its receipt, a settlement advances, or shutdown. `pop` and the settlement
            // check re-run at the top of the next iteration, so a signal between here and the drain
            // is not missed. Shutdown is checked first so a teardown request is never starved by a
            // busy watch and the worker's join never hangs. The batch-publication wake lets a
            // configured minimum bundle size (`*bundle_size.start() > 1`) self-heal: the ready
            // prefix grows past a parked point only when a batch behind the front publishes, which
            // nothing else notifies the loop of. The settlement wake is low-priority (last): a
            // pending re-aggregation is cheap and can wait behind real proving work.
            let shutdown = &self.prover.shutdown;
            let inbox = &self.prover.inbox;
            let queued = &self.queued;
            let settlement = self.settlement.as_mut();
            tokio::select! {
                biased;
                () = shutdown.wait() => break,
                () = inbox.notified() => {}
                () = next_queued_batch_published(queued) => {}
                () = settlement_changed(settlement) => {}
            }
        }
    }

    /// Forms the next bundle from the consecutively-ready prefix of the queue and proves it.
    /// Returns `true` when a bundle was consumed (proved, no-op, or empty), `false`
    /// when there was nothing to do.
    async fn try_prove_one_bundle(&mut self) -> bool {
        let Some(front) = self.queued.front().cloned() else {
            return false;
        };

        // Block on the first batch's receipt, but stay cancelable: on shutdown the batch prover may
        // never publish a receipt we are waiting on, which would otherwise deadlock the join.
        tokio::select! {
            biased;
            () = self.prover.shutdown.wait() => return false,
            () = front.wait_artifact_published() => {}
        }

        // A rollback may have canceled the front while we were parked: a canceled batch's wait
        // returns immediately, and cancellation force-opens the artifact latch without a receipt.
        // Evict the canceled prefix rather than bundle it (which would panic collecting a missing
        // receipt). Returning progress re-drains the inbox, where the forthcoming
        // `Command::Rollback` truncates `retained`, resets the re-form guard, and re-runs this
        // eviction on `queued` idempotently.
        if front.canceled() {
            while self.queued.front().is_some_and(|b| b.canceled()) {
                self.queued.pop_front();
            }
            return true;
        }

        // Greedily extend the bundle over the consecutively-ready prefix: include each following
        // batch whose receipt is published, stopping at the first that is not, at a canceled batch,
        // or at the configured maximum. Check artifact_published() before canceled(): a receiptless
        // force-opened latch always has canceled() already true, so this order never extends over a
        // batch that would panic when its missing receipt is collected.
        let mut take = 1;
        while take < self.queued.len()
            && take < *self.bundle_size.end()
            && self.queued[take].artifact_published()
            && !self.queued[take].canceled()
        {
            take += 1;
        }
        // Park until at least the configured minimum are consecutively ready. Returning false
        // leaves the batches queued; the run loop re-tries when more arrive. The extend
        // loop above caps `take` at the maximum, so a `take` short of the minimum is
        // genuinely short of ready batches, not the cap. With the default `1..`, `take >= 1
        // == *start()` always, so this never fires and behavior is identical to
        // greedy-from-1.
        if take < *self.bundle_size.start() {
            return false;
        }
        let bundle: Vec<ScheduledBatch<S, P>> =
            (0..take).map(|_| self.queued.pop_front().unwrap()).collect();

        self.prove_bundle(&bundle).await;
        // Retain the consumed batches so a competitor settling a shorter range can re-aggregate the
        // surviving suffix from them, but only when a settlement watch drives that re-aggregation:
        // with no watch wired, `retained` is never read and would grow for the whole run. A
        // shutdown-discard (the proof was abandoned mid-flight) skips retention, matching the
        // discard-the-proved-bundle behavior in `prove_bundle`.
        if self.settlement.is_some() && !self.prover.shutdown.is_open() {
            self.retained.extend(bundle.iter().cloned());
        }
        true
    }

    /// Aggregates one already-chosen bundle into a settlement receipt and publishes it: fetches the
    /// final block's lane proof, encodes the aggregator inputs over the per-batch journals, proves
    /// (with the per-batch receipts as composition assumptions) or reuses the cached receipt, then
    /// fills the published handle with the proved [`SettlementArtifact`]. An all-empty or no-op
    /// bundle (one that advances no state) publishes a resolved no-op handle instead. The same body
    /// serves the normal front-of-queue bundle and a re-aggregated superseded suffix.
    async fn prove_bundle(&self, bundle: &[ScheduledBatch<S, P>]) {
        let take = bundle.len();
        let last_checkpoint = bundle.last().unwrap().checkpoint();
        let last_metadata = *last_checkpoint.metadata();
        let block_prove_to = last_metadata.hash;

        // Bundle-start coordinate (first batch's index + block) keys the aggregator receipt in the
        // proof-receipt store.
        let first_checkpoint = bundle.first().unwrap().checkpoint();
        let checkpoint_index = first_checkpoint.index();
        let from_block = first_checkpoint.metadata().hash;

        // Empty batches publish no receipt; the aggregator composes only the non-empty ones.
        let receipts: Vec<B::Receipt> = bundle
            .iter()
            .filter(|b| !b.txs().is_empty())
            .map(|b| (*b.artifact()).clone())
            .collect();

        // An all-empty prefix advances no state: consume it without proving (there are no receipts
        // to compose). Publish a resolved no-op handle so a paced consumer accounts for these
        // batches.
        if receipts.is_empty() {
            self.emit(ScheduledBundle::resolved_noop(
                take,
                checkpoint_index,
                BundleBlocks { from_block, block_prove_to },
            ));
            return;
        }

        // Publish the (still unproven) bundle handle before proving, mirroring how the scheduler
        // publishes a `ScheduledBatch` before the batch prover fills its receipt: the settlement
        // worker can pop the handle and reconcile pacing now, then await the artifact. The retained
        // `handle` is filled below once proving completes.
        let handle = ScheduledBundle::new(
            take,
            checkpoint_index,
            BundleBlocks { from_block, block_prove_to },
        );
        self.emit(handle.clone());

        // The bundle's `from -> to` coordinate (its start checkpoint + block, claimed tip
        // commitment) proves to the same settlement receipt, so a replay (including a flip reorg
        // back onto this fork) reuses the cached one instead of re-fetching the lane proof and
        // re-proving. The key combines the bundle's own start coordinate, the claimed tip
        // `seq_commit`, and the aggregator image id the backend proves with; the receipt store is
        // the prover's own cache handle (bound by the scheduler at construction).
        let seq_commit = last_metadata.seq_commit.as_bytes();
        let agg_key = handle.agg_key(*self.backend.aggregator_image_id(), seq_commit);
        let receipt_store = &self.prover.receipt_store;
        let receipt = match receipt_store.read_agg_receipt(agg_key).resolve().await {
            Some(receipt) => receipt,
            None => {
                // Aggregate the bundle: fetch the final block's lane proof, encode the aggregator
                // inputs over the per-batch journals, and prove with the per-batch receipts as
                // composition assumptions.
                let lane_proof = self
                    .lane_source
                    .fetch_lane_proof(LaneProofRequest {
                        block: block_prove_to,
                        lane_key: self.lane_key,
                    })
                    .await;
                let journals: Vec<Vec<u8>> = receipts.iter().map(|r| B::journal_bytes(r)).collect();
                let inputs = AggregatorInputs::encode(
                    self.backend.batch_image_id(),
                    &lane_proof,
                    journals.iter().map(|j| j.as_slice()),
                );
                let receipt = self.backend.prove_aggregator(&inputs, receipts).await;
                if self.prover.shutdown.is_open() {
                    // Shutting down: resolve the published handle as a no-op so a consumer awaiting
                    // its artifact is released rather than blocked on a latch that never opens, and
                    // drop the proved bundle (the same discard-on-shutdown behavior as before).
                    handle.publish_artifact(None);
                    return;
                }

                // Wait for the receipt to be durable before publishing the artifact, so a crash
                // never leaves a consumed-but-uncached settlement receipt.
                receipt_store.write_agg_receipt(agg_key, receipt.clone()).wait().await;
                receipt
            }
        };

        // Parse the settlement journal.
        let journal = B::journal_bytes(&receipt);
        let st = (&mut &journal[..])
            .array_as::<StateTransition>("state_transition")
            .expect("aggregator journal");

        // A no-op bundle (no lane activity in its blocks) leaves the state unchanged: nothing to
        // settle. Resolve the published handle as a no-op so a paced consumer accounts for these
        // batches.
        if st.new_state == st.prev_state {
            handle.publish_artifact(None);
            return;
        }

        if let Some(covenant_id) = self.covenant_id {
            assert_eq!(
                Hash::from_bytes(st.covenant_id),
                covenant_id,
                "bundle journal covenant_id must match the configured covenant",
            );
        }
        debug_assert_eq!(
            st.new_seq_commit, last_metadata.seq_commit,
            "bundle new_seq_commit must equal the final block's seq_commit",
        );

        // Fill the published handle with the proved settlement; the settlement worker awaiting it
        // is then released. With no queue wired the bundle is proved but not settled (exec/test
        // paths).
        log::info!("aggregate-prover: proved bundle through {block_prove_to} (size {take})");
        let artifact = SettlementArtifact {
            receipt,
            block_prove_to,
            prev_state: st.prev_state,
            prev_lane_tip: st.prev_lane_tip,
            new_state: st.new_state,
            new_lane_tip: st.new_lane_tip,
            new_seq_commit: st.new_seq_commit,
            permission_spk_hash: st.permission_spk_hash,
            covenant_id: st.covenant_id,
        };
        handle.publish_artifact(Some(artifact));
    }

    /// Publishes a formed bundle's handle onto the settlement queue, if one is wired. With no queue
    /// the prover runs without settling and the handle is dropped.
    fn emit(&self, bundle: ScheduledBundle<SettlementArtifact<B::Receipt>>) {
        if let Some(queue) = &self.settlement_queue {
            queue.push(bundle);
        }
    }

    /// Re-aggregates the suffix of our retained batches that survives a competitor's settlement.
    ///
    /// `latest` is the bridge's newest covenant `last_settlement`. Both provers consume the same
    /// bridge batch stream, so the settlement's `block_prove_to` is the final block of one of our
    /// retained batches: drop that batch and every batch before it (the competitor covered them),
    /// then re-form the surviving suffix into a fresh bundle whose first batch's `prev_state`
    /// already equals the adopted tip, and prove it. The settler accepts that artifact directly
    /// (its `prev_state == cov.state`), so two contending provers converge on one continuation
    /// chain. Only the cheap aggregator STARK re-runs; the cached per-batch receipts are reused.
    ///
    /// Cases on the settlement boundary `block_prove_to`:
    /// - matches a retained batch: prefix-drain `0..=k` (keeps the suffix consecutive for the
    ///   verifier's `prev_state` chaining), then re-form the remainder.
    /// - absent from our window (the boundary is not one of our retained blocks): drop nothing and
    ///   re-form nothing. `block_prove_to` is a block hash with no orderable relation to our
    ///   retained blocks, so we cannot tell "covered all of them" from "behind / not ours" without
    ///   risking dropping batches that are still unsettled. Forward-only: a later settlement whose
    ///   boundary does land on a retained block drains them, and a competitor settling past our
    ///   whole window simply leaves a bounded residual that never re-forms (the same memory profile
    ///   as the pre-existing unbounded-await case, under the single-miner / low-reorg assumption).
    ///
    /// `None` is a no-op: a reorg that orphaned the settlement publishes `None`, and the rollback
    /// command truncates `retained` ahead of any re-form (single-miner / low-reorg assumption,
    /// inherited from the settler).
    async fn reaggregate_superseded(&mut self, latest: Option<SettlementInfo>) {
        let Some(settlement) = latest else {
            return;
        };

        // Re-form only on a boundary that lands on one of our retained blocks: the competitor (or
        // our own settler) covered it and everything before it. An unmatched boundary drops
        // nothing, so an unsettled suffix is never silently lost.
        let blocks: Vec<Hash> =
            self.retained.iter().map(|b| b.checkpoint().metadata().hash).collect();
        let Some(drain) = settled_prefix(&blocks, settlement.block_prove_to) else {
            return;
        };
        self.retained.drain(0..drain);

        // Re-form the surviving suffix: take up to a full bundle's worth from the front of the
        // retained remainder. Its first batch's `prev_state` already equals the adopted tip, so the
        // proved artifact chains straight off the settlement. The batches stay in `retained`
        // (dropped only when a later settlement covers them), so a still-shorter competitor
        // re-forms again until convergence; the guard and the receipt cache keep that
        // idempotent.
        let suffix_len = self.retained.len().min(*self.bundle_size.end());
        if suffix_len == 0 {
            self.last_reformed_from = None;
            return;
        }
        let suffix: Vec<ScheduledBatch<S, P>> =
            self.retained.iter().take(suffix_len).cloned().collect();
        let suffix_from = suffix.first().unwrap().checkpoint().index();
        if self.last_reformed_from == Some(suffix_from) {
            return;
        }
        self.prove_bundle(&suffix).await;
        self.last_reformed_from = Some(suffix_from);
    }

    /// Drops queued and retained batches rolled back by a reorg, and resets the re-form guard so
    /// the next settlement re-aggregates against the rolled-back retained suffix. The active
    /// bundle's proof is awaited inline, so a rollback command is only applied between bundles
    /// and can never silently include a rolled-back suffix; aborting a proof already running on
    /// the GPU remains a TODO (same gap as the batch prover).
    fn apply_rollback(&mut self, target_index: u64) {
        self.queued.retain(|b| b.checkpoint().index() <= target_index);
        self.retained.retain(|b| b.checkpoint().index() <= target_index);
        self.last_reformed_from = None;
    }
}

/// Awaits the receipt publication of the first not-yet-published queued batch, the wake source the
/// park needs beyond the inbox. With a configured minimum bundle size the ready prefix can be short
/// of the minimum; a batch behind the front publishing its receipt is what extends it, yet that
/// publication does not touch the inbox. Without this arm a formable min-size bundle would strand
/// at an idle tip. Parks forever when every queued batch is already published or the queue is
/// empty: then only a new command can grow the prefix, which the inbox arm already wakes on.
///
/// Canceled batches are skipped: a canceled batch's `wait_artifact_published` returns immediately,
/// so awaiting one would busy-spin. The caller evicts a canceled front, so this arm parks past
/// canceled batches until the rollback command arrives.
async fn next_queued_batch_published<S: Store, P: Processor<S>>(
    queued: &VecDeque<ScheduledBatch<S, P>>,
) {
    match queued.iter().find(|batch| !batch.artifact_published() && !batch.canceled()) {
        Some(batch) => batch.wait_artifact_published().await,
        None => std::future::pending::<()>().await,
    }
}

/// Awaits the settlement watch's next change, or parks forever when no watch is wired. The watch
/// also errors here once the bridge dropped the sender (node teardown); treat that as no further
/// settlements and park, leaving shutdown to drive the loop.
async fn settlement_changed(rx: Option<&mut watch::Receiver<Option<SettlementInfo>>>) {
    match rx {
        Some(rx) => {
            if rx.changed().await.is_err() {
                std::future::pending::<()>().await
            }
        }
        None => std::future::pending::<()>().await,
    }
}

/// How many leading retained blocks a settlement landing on `boundary` covers, given the retained
/// blocks in scheduling order.
///
/// `Some(n)`: `boundary` is the `n`-th retained block, so the settlement covered the first `n`;
/// drain that prefix and re-form the remainder. `None`: `boundary` is not one of our retained
/// blocks, so drop nothing. Dropping on an unmatched boundary would risk discarding a still-
/// unsettled suffix (the boundary may sit behind our window, or cover a range we never proved),
/// which is the wedge [`Worker::reaggregate_superseded`] exists to prevent. The boundary is matched
/// from the back: chain-block hashes are unique, so at most one retained block matches.
fn settled_prefix(retained_blocks: &[Hash], boundary: Hash) -> Option<usize> {
    retained_blocks.iter().rposition(|hash| *hash == boundary).map(|index| index + 1)
}

#[cfg(test)]
mod tests {
    use kaspa_hashes::Hash;

    use super::settled_prefix;

    fn block(byte: u8) -> Hash {
        Hash::from_bytes([byte; 32])
    }

    #[test]
    fn boundary_inside_window_drains_through_it() {
        let blocks = [block(1), block(2), block(3), block(4)];
        // A competitor settled through block 2: drain blocks 1 and 2, leaving [3, 4] to re-form.
        assert_eq!(settled_prefix(&blocks, block(2)), Some(2));
    }

    #[test]
    fn boundary_at_window_tip_drains_everything() {
        let blocks = [block(1), block(2), block(3)];
        assert_eq!(settled_prefix(&blocks, block(3)), Some(3));
    }

    #[test]
    fn unmatched_boundary_drains_nothing() {
        // The boundary is not one of our retained blocks: drop nothing rather than clear the
        // window, or an unsettled suffix is lost and the chain wedges.
        let blocks = [block(1), block(2), block(3)];
        assert_eq!(settled_prefix(&blocks, block(9)), None);
    }

    #[test]
    fn empty_window_drains_nothing() {
        assert_eq!(settled_prefix(&[], block(1)), None);
    }
}
