use std::{
    collections::VecDeque,
    thread::{JoinHandle, spawn},
};

use kaspa_hashes::Hash;
use tokio::runtime::Builder;
use vprogs_core_atomics::AsyncQueue;
use vprogs_core_codec::Reader;
use vprogs_core_smt::EMPTY_HASH;
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
    /// Batches accumulated but not yet bundled, in scheduling order.
    queued: VecDeque<ScheduledBatch<S, P>>,
    /// Last settled L1 block (the lower bound a new bundle chains from). `None` at genesis.
    from_block: Option<Hash>,
    /// Last settled L2 SMT state. The empty SMT at genesis.
    from_state: [u8; 32],
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
        let AggregateProverConfig { lane_key, covenant_id, lane_source, settlement_queue } = config;
        let this = Self {
            prover,
            backend,
            lane_key,
            covenant_id,
            lane_source,
            settlement_queue,
            queued: VecDeque::new(),
            from_block: None,
            from_state: EMPTY_HASH,
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

            // Fold in any external settlement visible on the queued batches, then try to prove one
            // bundle. Loop without parking while progress is made so back-to-back ready bundles
            // drain promptly.
            self.absorb_external_settlements();
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
    /// Returns `true` when a bundle was consumed (proved, settled-redundant, or empty), `false`
    /// when there was nothing to do.
    async fn try_prove_one_bundle(&mut self) -> bool {
        // A newly-formed bundle must not re-prove already-settled batches.
        self.drop_settled_prefix();

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

        // A settlement may have landed while we awaited; if it covered this work, the front is
        // dropped and there may be nothing left to prove.
        self.absorb_external_settlements();
        if self.queued.is_empty() {
            return true;
        }

        // Greedily extend the bundle over the consecutively-ready prefix: the front is ready
        // (awaited above); include each following batch whose receipt is already published, and
        // stop at the first one that is not.
        let mut take = 1;
        while take < self.queued.len() && self.queued[take].artifact_published() {
            take += 1;
        }
        let bundle: Vec<ScheduledBatch<S, P>> =
            (0..take).map(|_| self.queued.pop_front().unwrap()).collect();

        let last_metadata = *bundle.last().unwrap().checkpoint().metadata();
        let block_prove_to = last_metadata.hash;

        // Bundle-start coordinate (first batch's index + block) keys the aggregator receipt in the
        // proof-receipt store; `latest_settlement` is the newest on-chain covenant settlement as of
        // the final block, letting the settler skip a bundle an external settlement already
        // covered.
        let first_checkpoint = bundle.first().unwrap().checkpoint();
        let checkpoint_index = first_checkpoint.index();
        let from_block = first_checkpoint.metadata().hash;
        let latest_settlement = last_metadata.last_settlement;

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
                latest_settlement,
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
            latest_settlement,
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

        // The journal is authoritative; assert the chain-derived bookkeeping agrees before
        // advancing, so a desync fails loudly and locally (mirrors the settler's asserts).
        debug_assert_eq!(
            st.prev_state, self.from_state,
            "bundle prev_state must chain from the last settled state",
        );
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

        // Advance the settled lower bound to this bundle. The next bundle's aggregator self-derives
        // its own prev_state from the lane proof and journals, so this is bookkeeping only: it lets
        // us drop already-settled batches and detect redundant external settlements.
        self.from_block = Some(block_prove_to);
        self.from_state = st.new_state;

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

    /// Folds the newest external covenant settlement visible on the queued batches into the settled
    /// lower bound, dropping batches it already covered. `last_settlement` is monotone forward
    /// along the chain, so the last advancing entry wins.
    fn absorb_external_settlements(&mut self) {
        let newest: Option<SettlementInfo> = self
            .queued
            .iter()
            .filter_map(|b| b.checkpoint().metadata().last_settlement)
            .rfind(|s| s.new_state != self.from_state);
        let Some(s) = newest else {
            return;
        };

        self.from_block = Some(s.block_prove_to);
        self.from_state = s.new_state;
        self.drop_settled_prefix();

        // TODO: cancel an in-flight aggregator proof made redundant by this settlement. A running
        // GPU proof can't be aborted (same gap as the batch prover); dropping the queued prefix and
        // the redundancy checks below prevent *starting* redundant work, and because proofs are
        // awaited inline a stale result is simply never published.
    }

    /// Drops queued batches at or below the settled L1 block. There is a 1:1 L1-block to L2-batch
    /// correspondence (the node schedules one batch per chain block), so the settled
    /// `block_prove_to` equals some queued batch's block hash; everything up to and including
    /// it is already settled.
    fn drop_settled_prefix(&mut self) {
        let Some(from_block) = self.from_block else {
            return;
        };
        let cut = self
            .queued
            .iter()
            .find(|b| b.checkpoint().metadata().hash == from_block)
            .map(|b| b.checkpoint().index());
        if let Some(cut) = cut {
            self.queued.retain(|b| b.checkpoint().index() > cut);
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
