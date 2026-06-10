use std::thread::spawn;

use tokio::runtime::Builder;
use vprogs_l1_types::ChainBlockMetadata;
use vprogs_scheduling_scheduler::{Processor, ScheduledBatch};
use vprogs_storage_types::Store;
use vprogs_zk_abi::batch_processor::Inputs as BatchInputs;

use crate::{Backend, BatchProver, BatchProverConfig, command::Command};

/// Background worker that drains the prover inbox and produces one ZK receipt per scheduled
/// batch. Each receipt commits a per-batch [`BatchTransition`] that the (out-of-band)
/// aggregator chains into a bundle settlement.
///
/// [`BatchTransition`]: vprogs_zk_abi::batch_processor::BatchTransition
pub(crate) struct Worker<S: Store, P: Processor<S>, B: Backend> {
    /// Shared prover state (inbox, shutdown).
    prover: BatchProver<S, P>,
    /// Backend used for proving.
    backend: B,
    /// Store for reading SMT state proofs.
    store: S,
    /// Static config (lane key, covenant id).
    config: BatchProverConfig,
}

impl<S: Store, P, B: Backend> Worker<S, P, B>
where
    P: Processor<
            S,
            TransactionArtifact = B::Receipt,
            BatchArtifact = B::Receipt,
            BatchMetadata = ChainBlockMetadata,
        >,
{
    /// Spawns the worker on a new thread with a single-threaded tokio runtime.
    pub(crate) fn spawn(
        prover: BatchProver<S, P>,
        backend: B,
        store: S,
        config: BatchProverConfig,
    ) {
        let this = Self { prover, backend, store, config };
        let runtime = Builder::new_current_thread().enable_all().build().expect("runtime");
        spawn(move || runtime.block_on(this.run()));
    }

    /// Main loop: drain commands and prove each scheduled batch in arrival order.
    async fn run(mut self) {
        loop {
            // Apply commands from the inbox to local state.
            while let Some(cmd) = self.prover.inbox.pop() {
                match cmd {
                    Command::Batch(batch) => self.process_batch(batch).await,
                    Command::Rollback(_target) => {
                        // Per-batch proofs aren't held in a pending queue, so a rollback has
                        // nothing local to discard here. In-flight receipts that arrive for
                        // canceled batches are published anyway: the scheduler's `canceled`
                        // flag prevents them from being consumed downstream.
                    }
                }
            }

            tokio::select! {
                biased;
                () = self.prover.shutdown.wait() => break,
                () = self.prover.inbox.notified() => {}
            }
        }
    }

    /// Proves one scheduled batch and publishes the receipt as the batch's artifact.
    async fn process_batch(&mut self, batch: ScheduledBatch<S, P>) {
        // Wait for tx artifacts on the batch before proving (composition needs them).
        batch.wait_tx_artifacts_published().await;
        if batch.canceled() {
            return;
        }

        // The per-batch SMT proof is scoped to exactly this batch's resources, so the proof's
        // member indices line up 1:1 with the per-batch `resource_index` the tx-processor
        // commits -- no translation table needed.
        let batch_resources = batch.resource_ids();

        // ONE SMT walk per batch, at the version preceding the batch's checkpoint.
        let prev_version = batch.checkpoint().index().saturating_sub(1);
        let proof_bytes = self.store.prove(&batch_resources, prev_version).expect("proof");

        // Collect per-tx journal bytes from the batch's tx artifacts.
        let tx_journals: Vec<Vec<u8>> =
            batch.tx_artifacts().map(|a| B::journal_bytes(&a)).collect();

        // Pulled from the prover config: the covenant id this batch's journal binds to. Every
        // batch in a bundle MUST share the same covenant id (the aggregator asserts it).
        // Configs that don't anchor to a real covenant leave this `None`, committing the
        // all-zero placeholder.
        let covenant_id = self.config.covenant_id.map(|h| h.as_bytes()).unwrap_or_default();

        let input_bytes = BatchInputs::encode(
            (self.backend.image_id(), &covenant_id, &self.config.lane_key),
            &proof_bytes,
            batch.checkpoint().metadata(),
            &tx_journals,
        );

        // Compose against this batch's tx receipts.
        let tx_receipts: Vec<_> = batch.tx_artifacts().map(|a| (*a).clone()).collect();
        let receipt = self.backend.prove_batch(&input_bytes, tx_receipts).await;

        // Publish the receipt as the batch's artifact. Reuses the existing scheduler slot:
        // settlement / aggregation reads the per-batch receipts from there.
        batch.publish_artifact(Some(receipt));
    }
}
