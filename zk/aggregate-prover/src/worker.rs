use std::{
    collections::VecDeque,
    ops::RangeInclusive,
    sync::Arc,
    thread::{JoinHandle, spawn},
};

use kaspa_hashes::Hash;
use tokio::{
    runtime::Builder,
    sync::{mpsc, watch},
};
use vprogs_core_atomics::AsyncQueue;
use vprogs_core_codec::Reader;
use vprogs_l1_types::{ChainBlockMetadata, SettlementInfo};
use vprogs_scheduling_scheduler::{Processor, ScheduledBatch};
use vprogs_state_proof_receipt::{AggregatorKey, BatchKey, Prefix};
use vprogs_state_settlement_journal::{JournalEntry, SettlementJournal};
use vprogs_storage_types::Store;
use vprogs_zk_abi::batch_aggregator::{Inputs as AggregatorInputs, StateTransition};
use vprogs_zk_batch_prover::{LaneProofRequest, LaneProofSource};

use crate::{
    AggregateProver, AggregateProverConfig, Backend, BundleBlocks, ExitsForBundle, ScheduledBundle,
    SettlementArtifact, command::Command, extract_bundle_exits,
};

/// Outcome of one bundle prove attempt.
enum BundleOutcome {
    /// A handle was published onto the settlement queue (proved or resolved no-op).
    Emitted,
    /// The lane-proof fetch failed; the caller re-queues the bundle and retries after the next
    /// wake. Nothing was emitted, so no settlement consumer is waiting on it.
    Deferred,
}

/// Outcome of the prove-or-cache step inside one bundle attempt.
enum ProveOutcome<R> {
    /// The aggregate receipt (freshly proved or reloaded from cache).
    Receipt(R),
    /// Shutdown raced the proof; the caller discards the bundle.
    Shutdown,
    /// The lane-proof fetch failed for the bundle's final block.
    LaneProofFailed,
}

/// Outcome of one committed-gap pass.
enum GapOutcome {
    /// A bundle was re-formed over (part of) the committed range and recorded through this end
    /// index.
    Covered(u64),
    /// No re-formable range: nothing committed past the journal tail, the on-chain tip already
    /// covers it, or a deterministic metadata or receipt miss leaves it uncovered. Terminal, so
    /// the caller never retries.
    Nothing,
    /// The final block's lane proof could not be fetched (a dead block or a stalled node; the
    /// error does not say which). Carries the committed tip this attempt was scoped to; the
    /// caller retries on later loop wakes, re-bounded to it so the retry never grows over
    /// batches committed after the restart (those belong to the live bundling path).
    Deferred(u64),
}

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
    /// Batches consumed into a proved bundle but not yet covered by a settlement; re-formed by
    /// [`reaggregate_superseded`](Self::reaggregate_superseded) when a competitor supersedes them.
    retained: VecDeque<ScheduledBatch<S, P>>,
    /// Receiver on the bridge's covenant `last_settlement` watch driving
    /// [`reaggregate_superseded`](Self::reaggregate_superseded), or `None` to run without
    /// re-forming.
    settlement: Option<watch::Receiver<Option<SettlementInfo>>>,
    /// Journal of proved-but-unsettled bundles; `None` disables resume.
    journal: Option<Arc<dyn SettlementJournal>>,
    /// First-batch checkpoint index of the most recently re-formed suffix, guarding against
    /// re-emitting it on every settlement wake. Reset by a rollback.
    last_reformed_from: Option<u64>,
    /// Sender on the exit-leaf channel driving client Merkle-path proof generation, or `None` if
    /// exit publishing is disabled.
    exits: Option<mpsc::UnboundedSender<Arc<ExitsForBundle>>>,
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
            journal,
            bundle_size,
            exits,
        } = config;
        // Bundle formation parks while `take` is below the range start and caps `take` at the range
        // end, so an empty range (start > end) would never form a bundle. Reject it up front rather
        // than stall silently.
        assert!(
            !bundle_size.is_empty(),
            "bundle_size must be a non-empty range (start <= end); got {bundle_size:?}",
        );
        // The journal is the restart-resume half of settlement: without the queue (the worker
        // that settles) and the watch (compaction against the covenant tip) its entries would
        // only accumulate.
        assert!(
            journal.is_none() || (settlement_queue.is_some() && settlement.is_some()),
            "journal requires the full settling path (queue + watch); exec/test paths stay \
             journal-free",
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
            journal,
            last_reformed_from: None,
            exits,
        };
        let runtime = Builder::new_current_thread().enable_all().build().expect("runtime");
        spawn(move || runtime.block_on(this.run()))
    }

    /// Main loop: drain commands into local state, prove every ready bundle in arrival order, and
    /// re-aggregate a superseded suffix whenever the settlement watch advances.
    async fn run(mut self) {
        // Resume before any proving: if the journal holds pending bundles, wait for the bridge's
        // first tip publication (chain replay republishes the covenant's last settlement; a
        // covenant that never settled escapes on the first scheduled batch instead, see the
        // gate below), snapshot the pre-restart tail's span, and re-feed the tail ahead of new
        // work. The snapshot scopes the multi-pass resume: the first publication is the bridge's
        // PRE-downtime baseline (the persisted tip's last settlement), so a competitor that
        // settled during the downtime reaches the watch only as a later advance, and each
        // settlement advance below re-runs the resume pass against the snapshot until the
        // pre-restart tail is settled or dropped. A journal-free run skips straight through.
        //
        // After the tail resume, the committed-gap pass covers batches committed past the
        // journal tail; an empty journal over committed work reaches it too (the kill preceded
        // every journal record). A pass whose lane-proof fetch fails defers: `gap_bound` below
        // holds the committed tip it was scoped to, and the main loop retries against it.
        let mut resume_max_end = 0u64;
        // The committed-gap entry this run records (0 when none); the advance pass's scope
        // extends to it below, so a settlement boundary observed only after the gap pass
        // re-splits that entry exactly as it re-splits the pre-restart tail.
        let mut gap_end = 0u64;
        // The committed tip a deferred gap pass stays bounded to (`None` when no retry is
        // pending). The startup pass runs unbounded; every retry re-runs against the same
        // bound so it can only shrink (a boundary observed meanwhile splits the range) and
        // never grow over post-restart commits.
        let mut gap_bound: Option<u64> = None;
        let journal_holds_entries = self.journal.as_ref().is_some_and(|j| j.has_entries());
        if journal_holds_entries
            || self.journal.as_ref().is_some_and(|j| j.committed_tip().is_some())
        {
            // The gate escapes on the first scheduled batch when no tip ever comes: a covenant
            // whose first-ever settlement never landed (a TN5-shaped eviction striking bundle
            // one, a lane too fresh to have settled) has no last settlement for the bridge to
            // republish, so without the escape the gate parks the whole worker until shutdown,
            // holding both the tail re-feed and all new proving. The escape takes the no-tip
            // path: nothing on chain covers any entry, so the tail re-feeds unchanged, the
            // sibling of the unmapped-boundary path in [`resume_pending`](Self::resume_pending).
            // The bridge publishes its startup baseline before feeding any block, so when a tip
            // exists it is already current when the escape fires; a settlement racing the escape
            // reaches the main loop's advance pass below, which re-runs the resume with the real
            // tip. With no tip and no batch ever arriving (a bridge-only deployment, a dead
            // lane) the wait parks until shutdown, holding a re-formable gap that settles no
            // earlier than the first new activity, which is also the pre-fix behavior.
            // `Ok` only: an errored `changed` (the bridge dropped the sender, node teardown)
            // disables the arm, parking until shutdown rather than treating teardown as a tip.
            let tip = loop {
                let settlement = self.settlement.as_mut().expect("journal implies watch");
                if let Some(tip) = *settlement.borrow() {
                    break Some(tip);
                }
                tokio::select! {
                    biased;
                    () = self.prover.shutdown.wait() => return,
                    () = self.prover.inbox.notified() => break None,
                    Ok(()) = settlement.changed() => {}
                }
            };
            if journal_holds_entries {
                resume_max_end = self
                    .journal
                    .as_ref()
                    .expect("journal checked present above")
                    .entries()
                    .last()
                    .expect("has_entries checked above")
                    .1
                    .end_index;
                self.resume_pending(tip.as_ref(), resume_max_end).await;
                if self.prover.shutdown.is_open() {
                    return;
                }
            }
            // The pass derives the range's lower edge from the journal tail itself: with
            // entries it is the tail's end, and an empty journal over committed batches (the
            // kill preceded every bundle's journal record) starts the span at 0.
            match self.reform_committed_gap(tip.as_ref(), u64::MAX).await {
                GapOutcome::Covered(end) => gap_end = end,
                GapOutcome::Deferred(bound) => gap_bound = Some(bound),
                GapOutcome::Nothing => {}
            }
            if self.prover.shutdown.is_open() {
                return;
            }
        }

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
                if let Some(tip) = &latest {
                    self.compact_journal(tip);
                    // The advance pass of the multi-pass resume: acts only while the
                    // pre-restart snapshot still holds unsettled entries, and self-gates to a
                    // no-op once the snapshot scope is empty or the journal is unwired. The
                    // scope extends to this run's gap entry (its end exceeds the snapshot),
                    // so a boundary the gap pass could not yet observe re-splits it in
                    // process instead of wedging until the next restart.
                    self.resume_pending(Some(tip), resume_max_end.max(gap_end)).await;
                }
                if self.prover.shutdown.is_open() {
                    return;
                }
            }

            // Retry a deferred committed-gap pass before forming any new bundle. The fetch
            // error conflates a reorged-away block with a node that was merely stalled at
            // startup, and nothing else ever re-schedules a committed batch, so giving up
            // after one attempt leaves the range uncovered for the life of the process
            // whenever the failure was the node: the same deferral a live bundle's dead lane
            // proof gets, retried on every wake (a new batch, a settlement advance, or
            // shutdown). The retry re-runs the whole pass against the current journal and
            // tip, so a boundary that landed meanwhile splits the range, and it
            // self-terminates without a fetch once the journal tail reaches the bound (the
            // gap was covered, or new work settled past a genuinely dead block). Running
            // before bundle formation keeps the recovered gap bundle ahead of new-work
            // bundles on the settlement queue, the ordering the settler's skip path assumes.
            if let Some(bound) = gap_bound {
                let tip = self.settlement.as_ref().and_then(|rx| *rx.borrow());
                match self.reform_committed_gap(tip.as_ref(), bound).await {
                    GapOutcome::Covered(end) => {
                        gap_end = gap_end.max(end);
                        gap_bound = None;
                    }
                    GapOutcome::Nothing => gap_bound = None,
                    GapOutcome::Deferred(bound) => gap_bound = Some(bound),
                }
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
    /// Returns `true` when progress was made (a bundle consumed, proved, no-op, or empty; a
    /// canceled prefix evicted), `false` when there was nothing to do or the bundle's lane proof
    /// was unavailable (deferred, its batches re-queued at the front).
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

        match self.prove_bundle(&bundle).await {
            BundleOutcome::Emitted => {
                // Retain the consumed batches so a competitor settling a shorter range can
                // re-aggregate the surviving suffix from them, but only when a settlement watch
                // drives that re-aggregation: with no watch wired, `retained` is never read and
                // would grow for the whole run. A shutdown-discard (the proof was abandoned
                // mid-flight) skips retention, matching the discard-the-proved-bundle behavior in
                // `prove_bundle`.
                if self.settlement.is_some() && !self.prover.shutdown.is_open() {
                    self.retained.extend(bundle.iter().cloned());
                }
                true
            }
            BundleOutcome::Deferred => {
                // Put the bundle back at the front and park: the next wake (a new batch from a
                // live L1, the rollback command a reorg is about to deliver, or shutdown)
                // re-drives formation against whatever the chain looks like then. Returning false
                // parks the loop instead of busy-spinning the fetch. The retry cadence is bound
                // to new L1 activity (inbox wakes) with no dedicated timer; add one only if a
                // stalled node must be re-polled while the lane is otherwise idle.
                for batch in bundle.iter().rev() {
                    self.queued.push_front(batch.clone());
                }
                false
            }
        }
    }

    /// Proves one already-chosen bundle and publishes its handle with the settled
    /// [`SettlementArtifact`], or a resolved no-op handle when the bundle is all-empty or leaves
    /// the committed transition unchanged. A bundle whose coordinate has proved before reuses its
    /// cached receipt instead of re-proving. The handle is emitted only once a receipt exists: a
    /// bundle whose lane-proof fetch fails is returned as [`BundleOutcome::Deferred`] with nothing
    /// emitted, so no settlement consumer is left waiting on it.
    async fn prove_bundle(&self, bundle: &[ScheduledBatch<S, P>]) -> BundleOutcome {
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
            return BundleOutcome::Emitted;
        }

        // The bundle's handle, emitted only after a receipt exists below: a fetch that fails (the
        // final block was reorged away) defers the bundle without publishing a handle no consumer
        // could ever see resolved.
        let handle = ScheduledBundle::new(
            take,
            checkpoint_index,
            BundleBlocks { from_block, block_prove_to },
        );

        // The bundle's `from -> to` coordinate (its start checkpoint + block, claimed tip
        // commitment) proves to the same settlement receipt, so a replay (including a flip reorg
        // back onto this fork) reuses the cached one instead of re-fetching the lane proof and
        // re-proving. The key combines the bundle's own start coordinate, the claimed tip
        // `seq_commit`, and the aggregator image id the backend proves with; the receipt store is
        // the prover's own cache handle (bound by the scheduler at construction).
        let seq_commit = last_metadata.seq_commit.as_bytes();
        let agg_key = handle.agg_key(*self.backend.aggregator_image_id(), seq_commit);
        let journals: Vec<Vec<u8>> = receipts.iter().map(|r| B::journal_bytes(r)).collect();
        let receipt = match self.prove_or_cache(agg_key, block_prove_to, receipts).await {
            ProveOutcome::Receipt(receipt) => receipt,
            // Shutdown raced the proof or the final block's lane proof is unavailable (a reorg
            // orphaned it): nothing was emitted, so no consumer can be waiting on a handle, and
            // the caller discards or defers the bundle.
            ProveOutcome::Shutdown | ProveOutcome::LaneProofFailed => {
                return BundleOutcome::Deferred;
            }
        };

        // Publish the bundle handle now that its receipt exists, mirroring how the scheduler
        // publishes a `ScheduledBatch` before the batch prover fills its receipt: the settlement
        // worker can pop the handle and reconcile pacing, then await the artifact. The retained
        // `handle` is filled below.
        self.emit(handle.clone());

        // Parse the settlement journal.
        let journal = B::journal_bytes(&receipt);
        let st = (&mut &journal[..])
            .array_as::<StateTransition>("state_transition")
            .expect("aggregator journal");

        // A no-op bundle leaves the whole committed transition unchanged: same state root, same
        // lane tip, no exits, no deposit. The parts move independently (a failed tx advances the
        // lane activity digest without writing a resource), so each is checked. Nothing to settle:
        // resolve the published handle as a no-op so a paced consumer accounts for these batches.
        if st.new_state == st.prev_state
            && st.new_lane_tip == st.prev_lane_tip
            && st.permission_spk_hash == [0u8; 32]
            && st.deposit_spk_hash == [0u8; 32]
        {
            handle.publish_artifact(None);
            return BundleOutcome::Emitted;
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
            deposit_spk_hash: st.deposit_spk_hash,
            covenant_id: st.covenant_id,
        };
        handle.publish_artifact(Some(artifact));

        // Record the published bundle's geometry so a restart can reload its receipt and re-feed
        // the bundle to settlement.
        if let Some(settlement_journal) = &self.journal {
            settlement_journal.record(
                checkpoint_index,
                &JournalEntry {
                    end_index: last_checkpoint.index(),
                    from_block,
                    block_prove_to,
                    seq_commit: last_metadata.seq_commit,
                },
            );
        }

        // Publish exit leaves for client Merkle-path generation when exits were emitted.
        if let Some(sender) = &self.exits {
            if st.permission_spk_hash != [0u8; 32] {
                let leaves =
                    Arc::new(extract_bundle_exits(&journals).expect("decode bundle exits"));
                // Receiver dropped means no consumer is listening; silently ignore.
                let _ = sender.send(Arc::new(ExitsForBundle {
                    new_state: st.new_state,
                    permission_spk_hash: st.permission_spk_hash,
                    leaves,
                }));
            }
        }
        BundleOutcome::Emitted
    }

    /// Proves (or reloads from cache) the aggregate receipt for a bundle proving through
    /// `block_prove_to` over the non-empty `receipts`, keying the cache at `agg_key`. Returns
    /// [`ProveOutcome::Shutdown`] when shutdown races the proof (the caller discards the bundle)
    /// or [`ProveOutcome::LaneProofFailed`] when the final block's lane proof cannot be fetched
    /// (the caller defers or drops the bundle).
    async fn prove_or_cache(
        &self,
        agg_key: AggregatorKey,
        block_prove_to: Hash,
        receipts: Vec<B::Receipt>,
    ) -> ProveOutcome<B::Receipt> {
        let receipt_store = &self.prover.receipt_store;
        if let Some(receipt) = receipt_store.read_agg_receipt(agg_key).resolve().await {
            return ProveOutcome::Receipt(receipt);
        }
        // Aggregate the bundle: fetch the final block's lane proof, encode the aggregator inputs
        // over the per-batch journals, and prove with the per-batch receipts as composition
        // assumptions.
        //
        // Stay cancelable while fetching: the remote source retries a dead node for up to ~105s
        // and each in-flight wRPC request holds the node's store Arc, so an uncanceled fetch
        // would wedge the shutdown join and keep a restarting node from reopening its store.
        // Dropping the fetch future aborts the request in milliseconds instead; Shutdown makes the
        // caller discard the bundle the same way as a proof abandoned mid-proof below.
        let journals: Vec<Vec<u8>> = receipts.iter().map(|r| B::journal_bytes(r)).collect();
        let fetched = tokio::select! {
            biased;
            () = self.prover.shutdown.wait() => return ProveOutcome::Shutdown,
            proof = self.lane_source.fetch_lane_proof(LaneProofRequest {
                block: block_prove_to,
                lane_key: self.lane_key,
            }) => proof,
        };
        let lane_proof = match fetched {
            Ok(proof) => proof,
            Err(e) => {
                log::warn!(
                    "aggregate-prover: lane proof for {block_prove_to} unavailable ({e}); \
                     deferring the bundle until the chain or the node recovers"
                );
                return ProveOutcome::LaneProofFailed;
            }
        };
        let inputs = AggregatorInputs::encode(
            self.backend.batch_image_id(),
            &lane_proof,
            journals.iter().map(|j| j.as_slice()),
        );
        let receipt = self.backend.prove_aggregator(&inputs, receipts).await;
        if self.prover.shutdown.is_open() {
            return ProveOutcome::Shutdown;
        }

        // Wait for the receipt to be durable before publishing the artifact, so a crash never
        // leaves a consumed-but-uncached settlement receipt.
        receipt_store.write_agg_receipt(agg_key, receipt.clone()).wait().await;
        ProveOutcome::Receipt(receipt)
    }

    /// Publishes a formed bundle's handle onto the settlement queue, if one is wired. With no queue
    /// the prover runs without settling and the handle is dropped.
    fn emit(&self, bundle: ScheduledBundle<SettlementArtifact<B::Receipt>>) {
        if let Some(queue) = &self.settlement_queue {
            queue.push(bundle);
        }
    }

    /// Re-aggregates the suffix of our retained batches that survives a competitor's settlement,
    /// so two contending provers converge on one continuation chain: drops the batches the
    /// settlement covered, then re-proves the remainder as a fresh bundle chaining off the adopted
    /// tip (only the cheap aggregator STARK re-runs; the cached per-batch receipts are reused).
    /// `latest: None` (a reorg orphaned the settlement) is a no-op.
    async fn reaggregate_superseded(&mut self, latest: Option<SettlementInfo>) {
        let Some(settlement) = latest else {
            return;
        };

        // A bundle that starts before the boundary chains its own lane-tip sequence, and the
        // first one to extend past it would carry a `prev_lane_tip` the covenant never took,
        // which the settler's build rejects (the catch-up wedge: the follower's bundling
        // boundaries need not match the settler's). Drain the boundary's prefix from both
        // windows so the next bundle formed from the queue starts strictly after it, its first
        // batch entering with the settlement's own state and lane tip.
        let boundary = settlement.block_prove_to;
        let queued_drain =
            settled_prefix(self.queued.iter().map(|b| b.checkpoint().metadata().hash), boundary);
        let retained_drain =
            settled_prefix(self.retained.iter().map(|b| b.checkpoint().metadata().hash), boundary);
        // An unmatched boundary drops nothing: with no orderable relation between the boundary
        // and our window blocks we cannot tell "covered all" from "behind / not ours", and
        // dropping would risk discarding a still-unsettled suffix. Forward-only, under the
        // single-miner / low-reorg assumption.
        if queued_drain.is_none() && retained_drain.is_none() {
            log::debug!(
                "aggregate-prover: settlement {} boundary {} matches no window block; nothing \
                 to drop",
                settlement.tx_id,
                boundary,
            );
            return;
        }
        if let Some(drain) = queued_drain {
            self.queued.drain(0..drain);
        }
        if let Some(drain) = retained_drain {
            self.retained.drain(0..drain);
        }

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
        // A deferral here (the suffix's final block is dead) drops the re-formed bundle: the
        // reorg's forthcoming rollback resets the re-form guard and truncates `retained`, so new
        // work re-drives whatever range survives.
        self.prove_bundle(&suffix).await;
        self.last_reformed_from = Some(suffix_from);
    }

    /// Deletes journal entries the on-chain settlement `tip` fully covers. A boundary mapping to
    /// no batch in the journal's span (a competitor's fork block outside our metadata) deletes
    /// nothing and logs.
    fn compact_journal(&self, tip: &SettlementInfo) {
        let Some(journal) = &self.journal else { return };
        let entries = journal.entries();
        let Some((first_start, _)) = entries.first() else { return };
        let Some((_, last)) = entries.last() else { return };
        let Some(tip_index) =
            journal.checkpoint_of_block(tip.block_prove_to, last.end_index, *first_start)
        else {
            log::warn!(
                "aggregate-prover: settlement {} boundary {} maps to no batch in the journal \
                 span; keeping {} entries",
                tip.tx_id,
                tip.block_prove_to,
                entries.len(),
            );
            return;
        };
        for (start, entry) in entries {
            if entry.end_index <= tip_index {
                journal.delete(start);
            }
        }
    }

    /// Resumes settlement after a restart: deletes journal entries the on-chain tip already
    /// covers, splits the one entry a competitor's boundary lands inside, and re-feeds every
    /// surviving entry onto the settlement queue ahead of new work. Re-fed bundles chain exactly
    /// like fresh ones; the settlement worker's adopt/skip/superseded paths land them.
    ///
    /// Scoped to entries with `end_index <= max_end`: startup snapshots the pre-restart journal
    /// tail, and each settlement-watch advance re-runs the pass against that snapshot until the
    /// tail settles (the bridge's first startup publication is the pre-downtime baseline, so a
    /// competitor that settled during the downtime reaches the watch only as a later advance).
    /// `tip: None` (no settlement ever landed) re-feeds the scoped tail unchanged. An entry
    /// whose receipt cannot be reloaded is dropped with a warning; that range settles again only
    /// through new activity.
    async fn resume_pending(&mut self, tip: Option<&SettlementInfo>, max_end: u64) {
        let Some(journal) = self.journal.clone() else { return };
        let entries: Vec<(u64, JournalEntry)> =
            journal.entries().into_iter().filter(|(_, entry)| entry.end_index <= max_end).collect();
        if entries.is_empty() {
            return;
        }
        let Some(tip) = tip else {
            log::info!(
                "aggregate-prover: resume has no on-chain settlement to anchor on; re-feeding \
                 the journal tail ({} entries) unchanged",
                entries.len()
            );
            self.refeed_all(entries).await;
            return;
        };
        let first_start = entries.first().expect("checked non-empty").0;
        let last_end = entries.last().expect("checked non-empty").1.end_index;
        let Some(tip_index) =
            journal.checkpoint_of_block(tip.block_prove_to, last_end, first_start)
        else {
            log::warn!(
                "aggregate-prover: resume tip boundary {} maps to no batch in the journal span \
                 ({} entries); re-feeding the tail unchanged",
                tip.block_prove_to,
                entries.len(),
            );
            self.refeed_all(entries).await;
            return;
        };

        let mut pending: Vec<(u64, JournalEntry)> = Vec::new();
        for (start, entry) in entries {
            if entry.end_index <= tip_index {
                journal.delete(start);
                log::info!(
                    "aggregate-prover: resume settled through checkpoint {tip_index}; dropping \
                     covered bundle {start}..={}",
                    entry.end_index
                );
            } else if start <= tip_index {
                self.split_straddler(start, entry, tip_index).await;
            } else {
                pending.push((start, entry));
            }
        }
        self.refeed_all(pending).await;
    }

    /// Startup pass covering committed-but-unjournaled batches: a kill between a batch's commit
    /// and its bundle's journal record leaves a checkpoint range the scheduler never re-schedules,
    /// so every later bundle would prove from a state root the covenant never took and the settler
    /// would skip it forever (the restarted-prover-idle wedge). Re-forms one bundle over that
    /// range from persisted batch metadata and cached per-batch receipts, records its entry, and
    /// feeds it after the journal tail, ahead of new work. Returns [`GapOutcome::Covered`] with
    /// the recorded entry's end index for the advance pass's scope, [`GapOutcome::Nothing`] when
    /// the range needs no cover, or [`GapOutcome::Deferred`] when the lane-proof fetch failed and
    /// the caller should retry on later wakes.
    ///
    /// `tip` bounds the range below: a settlement boundary landing inside it splits the range
    /// exactly as [`split_straddler`](Self::split_straddler) splits a journaled entry. `bound`
    /// caps the range above at the committed tip a prior attempt was scoped to (`u64::MAX` on a
    /// first run), so a retry never grows over batches committed after the restart; the range's
    /// lower edge is the journal tail, so coverage by any path advances it past the gap.
    async fn reform_committed_gap(
        &mut self,
        tip: Option<&SettlementInfo>,
        bound: u64,
    ) -> GapOutcome {
        let Some(journal) = self.journal.clone() else { return GapOutcome::Nothing };
        let tail_end = journal.entries().last().map_or(0, |(_, entry)| entry.end_index);
        let Some((committed_tip, _)) = journal.committed_tip() else { return GapOutcome::Nothing };
        let committed_tip = committed_tip.min(bound);
        if committed_tip <= tail_end {
            return GapOutcome::Nothing;
        }
        let boundary = match tip {
            Some(tip) => journal
                .checkpoint_of_block(tip.block_prove_to, committed_tip, tail_end + 1)
                .map_or(tail_end, |index| index.max(tail_end)),
            None => tail_end,
        };
        // The on-chain tip already covers the whole range; new work chains from it directly.
        if boundary >= committed_tip {
            return GapOutcome::Nothing;
        }
        let first = boundary + 1;
        let Some(first_metadata) = journal.batch_metadata(first) else {
            log::error!(
                "aggregate-prover: committed batch {first} above the journal tail lacks metadata; \
                 leaving its range uncovered"
            );
            return GapOutcome::Nothing;
        };
        // The bundle covers the contiguously-durable prefix: an empty batch is durable as-is,
        // and the first non-empty batch without metadata or a receipt bounds the range. The
        // entry's end fields derive from the last covered batch's own metadata, as the live
        // record derives them from the bundle's final batch.
        let mut receipts: Vec<B::Receipt> = Vec::new();
        let mut end_metadata = first_metadata;
        let mut covered_end = first;
        let mut miss = None;
        for index in first..=committed_tip {
            let Some(metadata) = journal.batch_metadata(index) else {
                miss = Some(index);
                break;
            };
            if metadata.lane_tip != metadata.prev_lane_tip {
                let key = BatchKey {
                    prefix: Prefix { checkpoint_index: index.into() },
                    block_hash: metadata.hash.as_bytes(),
                    image_id: *self.backend.batch_image_id(),
                };
                let Some(receipt) =
                    self.prover.receipt_store.read_batch_receipt(key).resolve().await
                else {
                    miss = Some(index);
                    break;
                };
                receipts.push(receipt);
            }
            end_metadata = metadata;
            covered_end = index;
        }
        if let Some(miss) = miss {
            // `covered_end` still sits at `first` when the miss IS the first index, so say
            // "nothing" rather than claim coverage through an index the loop never reached.
            let covered =
                if miss > first { format!("only through {covered_end}") } else { "nothing".into() };
            log::error!(
                "aggregate-prover: committed batch {miss} above the journal tail lacks its \
                 metadata or receipt; covering {covered} and leaving {miss}..={committed_tip} \
                 uncovered"
            );
        }
        // No real work below the miss: nothing to compose, matching the live no-op path.
        if receipts.is_empty() {
            return GapOutcome::Nothing;
        }
        let agg_key = AggregatorKey {
            prefix: Prefix { checkpoint_index: first.into() },
            block_hash: first_metadata.hash.as_bytes(),
            image_id: *self.backend.aggregator_image_id(),
            seq_commit: end_metadata.seq_commit.as_bytes(),
        };
        let receipt = match self.prove_or_cache(agg_key, end_metadata.hash, receipts).await {
            ProveOutcome::Receipt(receipt) => receipt,
            // Shutdown raced the proof; the caller exits the loop and the next startup re-runs
            // the pass.
            ProveOutcome::Shutdown => return GapOutcome::Nothing,
            ProveOutcome::LaneProofFailed => {
                log::warn!(
                    "aggregate-prover: committed-gap bundle through {} has no live lane proof \
                     (a dead block or a stalled node); deferring the range to the next wake",
                    end_metadata.hash
                );
                return GapOutcome::Deferred(committed_tip);
            }
        };
        let entry = JournalEntry {
            end_index: covered_end,
            from_block: first_metadata.hash,
            block_prove_to: end_metadata.hash,
            seq_commit: end_metadata.seq_commit,
        };
        journal.record(first, &entry);
        self.refeed_one(first, &receipt, &entry).await;
        log::info!(
            "aggregate-prover: re-formed committed gap {first}..={covered_end} onto the \
             settlement queue"
        );
        GapOutcome::Covered(covered_end)
    }

    /// Splits the journal entry a settlement boundary lands inside: re-aggregates its suffix
    /// strictly after `tip_index` from cached per-batch receipts, records the successor entry,
    /// and re-feeds it. Empty batches compose nothing; a missing receipt drops the entry with a
    /// warning instead of wedging.
    async fn split_straddler(&mut self, start: u64, entry: JournalEntry, tip_index: u64) {
        let Some(journal) = self.journal.clone() else { return };
        let successor_start = tip_index + 1;
        let mut suffix: Vec<B::Receipt> = Vec::new();
        for index in successor_start..=entry.end_index {
            let Some(metadata) = journal.batch_metadata(index) else {
                log::warn!(
                    "aggregate-prover: resume split lacks batch {index} metadata; dropping \
                     bundle {start}"
                );
                journal.delete(start);
                return;
            };
            if metadata.lane_tip == metadata.prev_lane_tip {
                continue;
            }
            let key = BatchKey {
                prefix: Prefix { checkpoint_index: index.into() },
                block_hash: metadata.hash.as_bytes(),
                image_id: *self.backend.batch_image_id(),
            };
            let Some(receipt) = self.prover.receipt_store.read_batch_receipt(key).resolve().await
            else {
                log::warn!(
                    "aggregate-prover: resume split lacks batch {index} receipt; dropping bundle \
                     {start}"
                );
                journal.delete(start);
                return;
            };
            suffix.push(receipt);
        }
        if suffix.is_empty() {
            journal.delete(start);
            return;
        }
        let from_block = journal.batch_block(successor_start).expect("read above");
        let receipts = suffix;
        let agg_key = AggregatorKey {
            prefix: Prefix { checkpoint_index: successor_start.into() },
            block_hash: from_block.as_bytes(),
            image_id: *self.backend.aggregator_image_id(),
            seq_commit: entry.seq_commit.as_bytes(),
        };
        let receipt = match self.prove_or_cache(agg_key, entry.block_prove_to, receipts).await {
            ProveOutcome::Receipt(receipt) => receipt,
            ProveOutcome::Shutdown => return, // shutdown mid-proof; the next startup re-runs
            // the pass
            ProveOutcome::LaneProofFailed => {
                log::warn!(
                    "aggregate-prover: resume split for bundle {start} has no live lane proof \
                     (reorg during downtime); dropping it"
                );
                journal.delete(start);
                return;
            }
        };
        let successor = JournalEntry {
            end_index: entry.end_index,
            from_block,
            block_prove_to: entry.block_prove_to,
            seq_commit: entry.seq_commit,
        };
        // Record the successor BEFORE deleting the original: record replaces by key and
        // tolerates overlap, so a crash between the two commits leaves both entries
        // (absorbed by the next resume's compact/split), never neither (which would
        // silently lose the suffix).
        journal.record(successor_start, &successor);
        journal.delete(start);
        self.refeed_one(successor_start, &receipt, &successor).await;
    }

    /// Re-feeds ordered journal entries as pre-proved bundles onto the settlement queue; an
    /// entry whose receipt fails to reload is dropped with a warning.
    async fn refeed_all(&mut self, entries: Vec<(u64, JournalEntry)>) {
        for (start, entry) in entries {
            let key = AggregatorKey {
                prefix: Prefix { checkpoint_index: start.into() },
                block_hash: entry.from_block.as_bytes(),
                image_id: *self.backend.aggregator_image_id(),
                seq_commit: entry.seq_commit.as_bytes(),
            };
            let Some(receipt) = self.prover.receipt_store.read_agg_receipt(key).resolve().await
            else {
                log::warn!(
                    "aggregate-prover: resume cannot reload receipt for bundle {start}; dropping it"
                );
                if let Some(journal) = &self.journal {
                    journal.delete(start);
                }
                continue;
            };
            self.refeed_one(start, &receipt, &entry).await;
            if self.prover.shutdown.is_open() {
                return;
            }
        }
    }

    /// Publishes one re-fed bundle from its reloaded receipt: decode the settlement transition,
    /// assert the covenant when bound, fill the handle, and push it onto the settlement queue.
    async fn refeed_one(&self, start: u64, receipt: &B::Receipt, entry: &JournalEntry) {
        let journal = B::journal_bytes(receipt);
        let st = (&mut &journal[..])
            .array_as::<StateTransition>("state_transition")
            .expect("aggregator journal");
        if let Some(covenant_id) = self.covenant_id {
            assert_eq!(
                Hash::from_bytes(st.covenant_id),
                covenant_id,
                "resumed bundle journal covenant_id must match the configured covenant",
            );
        }
        let handle = ScheduledBundle::new(
            (entry.end_index - start + 1) as usize,
            start,
            BundleBlocks { from_block: entry.from_block, block_prove_to: entry.block_prove_to },
        );
        handle.publish_artifact(Some(SettlementArtifact {
            receipt: receipt.clone(),
            block_prove_to: entry.block_prove_to,
            prev_state: st.prev_state,
            prev_lane_tip: st.prev_lane_tip,
            new_state: st.new_state,
            new_lane_tip: st.new_lane_tip,
            new_seq_commit: st.new_seq_commit,
            permission_spk_hash: st.permission_spk_hash,
            deposit_spk_hash: st.deposit_spk_hash,
            covenant_id: st.covenant_id,
        }));
        self.emit(handle);
        log::info!(
            "aggregate-prover: resumed bundle {start}..={} onto the settlement queue",
            entry.end_index
        );
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

/// Awaits the receipt publication of the first not-yet-published queued batch: the park arm
/// that wakes the run loop when a min-size bundle's ready prefix can grow without a new command.
/// Parks forever when every queued batch is already published or the queue is empty, leaving
/// waking to the inbox arm.
async fn next_queued_batch_published<S: Store, P: Processor<S>>(
    queued: &VecDeque<ScheduledBatch<S, P>>,
) {
    // Canceled batches are skipped: their `wait_artifact_published` returns immediately, so
    // awaiting one would busy-spin. The caller evicts a canceled front.
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

/// How many leading window blocks a settlement landing on `boundary` covers, given the window's
/// blocks in scheduling order: `Some(n)` when `boundary` is the `n`-th window block, `None` when
/// it matches none of them.
fn settled_prefix(
    mut blocks: impl DoubleEndedIterator<Item = Hash> + ExactSizeIterator,
    boundary: Hash,
) -> Option<usize> {
    blocks.rposition(|hash| hash == boundary).map(|index| index + 1)
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
        assert_eq!(settled_prefix(blocks.iter().copied(), block(2)), Some(2));
    }

    #[test]
    fn boundary_at_window_tip_drains_everything() {
        let blocks = [block(1), block(2), block(3)];
        assert_eq!(settled_prefix(blocks.iter().copied(), block(3)), Some(3));
    }

    #[test]
    fn unmatched_boundary_drains_nothing() {
        // The boundary is not one of our retained blocks: drop nothing rather than clear the
        // window, or an unsettled suffix is lost and the chain wedges.
        let blocks = [block(1), block(2), block(3)];
        assert_eq!(settled_prefix(blocks.iter().copied(), block(9)), None);
    }

    #[test]
    fn empty_window_drains_nothing() {
        assert_eq!(settled_prefix(std::iter::empty(), block(1)), None);
    }
}
