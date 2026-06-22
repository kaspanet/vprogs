use std::{
    collections::VecDeque,
    ops::RangeInclusive,
    thread::{JoinHandle, spawn},
};

use kaspa_hashes::Hash;
use tokio::runtime::Builder;
use vprogs_core_atomics::AsyncQueue;
use vprogs_core_codec::Reader;
use vprogs_l1_types::ChainBlockMetadata;
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
        };
        let runtime = Builder::new_current_thread().enable_all().build().expect("runtime");
        spawn(move || runtime.block_on(this.run()))
    }

    /// Main loop: drain commands into local state, then prove every ready bundle in arrival order.
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

            // Try to prove one bundle. Loop without parking while progress is made so back-to-back
            // ready bundles drain promptly.
            let made_progress = self.try_prove_one_bundle().await;
            if self.prover.shutdown.is_open() {
                return;
            }
            if made_progress {
                continue;
            }

            // Nothing ready: park until a new command arrives or shutdown. `pop` re-runs at the top
            // of the next iteration, so a push between here and the drain is not missed.
            tokio::select! {
                biased;
                () = self.prover.shutdown.wait() => break,
                () = self.prover.inbox.notified() => {}
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

        // Greedily extend the bundle over the consecutively-ready prefix: the front is ready
        // (awaited above); include each following batch whose receipt is already published, and
        // stop at the first one that is not, or once the configured maximum is reached.
        let mut take = 1;
        while take < self.queued.len()
            && take < *self.bundle_size.end()
            && self.queued[take].artifact_published()
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
            return true;
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
        // re-proving. The bundle keys the receipt off its own start coordinate and the claimed tip
        // `seq_commit`; its first batch is the storage gateway (the aggregate prover holds no store
        // of its own) and supplies the aggregator image id.
        let seq_commit = last_metadata.seq_commit.as_bytes();
        let first_batch = bundle.first().unwrap();
        let receipt = match handle.read_agg_receipt(first_batch, seq_commit).resolve().await {
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
                    return true;
                }

                // Wait for the receipt to be durable before publishing the artifact, so a crash
                // never leaves a consumed-but-uncached settlement receipt.
                handle.write_agg_receipt(first_batch, seq_commit, receipt.clone()).wait().await;
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
            return true;
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
        true
    }

    /// Publishes a formed bundle's handle onto the settlement queue, if one is wired. With no queue
    /// the prover runs without settling and the handle is dropped.
    fn emit(&self, bundle: ScheduledBundle<SettlementArtifact<B::Receipt>>) {
        if let Some(queue) = &self.settlement_queue {
            queue.push(bundle);
        }
    }

    /// Drops queued batches rolled back by a reorg. The active bundle's proof is awaited inline, so
    /// a rollback command is only applied between bundles and can never silently include a
    /// rolled-back suffix; aborting a proof already running on the GPU remains a TODO (same gap as
    /// the batch prover).
    fn apply_rollback(&mut self, target_index: u64) {
        self.queued.retain(|b| b.checkpoint().index() <= target_index);
    }
}
