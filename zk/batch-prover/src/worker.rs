use std::thread::{JoinHandle, spawn};

use tokio::runtime::Builder;
use vprogs_l1_types::ChainBlockMetadata;
use vprogs_scheduling_scheduler::{Processor, ScheduledBatch};
use vprogs_storage_types::Store;
use vprogs_zk_abi::batch_processor::{BatchPins, Inputs as BatchInputs};

use crate::{Backend, BatchProver, BatchProverConfig, command::Command};

/// Background worker that drains the prover inbox and produces one ZK receipt per scheduled batch.
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
    /// Spawns the worker on a new thread with a single-threaded tokio runtime and returns its join
    /// handle. The prover joins this on shutdown so the worker's GPU prover is torn down (its risc0
    /// CUDA context released) before the process exits.
    pub(crate) fn spawn(
        prover: BatchProver<S, P>,
        backend: B,
        store: S,
        config: BatchProverConfig,
    ) -> JoinHandle<()> {
        let this = Self { prover, backend, store, config };
        let runtime = Builder::new_current_thread().enable_all().build().expect("runtime");
        spawn(move || runtime.block_on(this.run()))
    }

    /// Main loop: drain commands and prove each scheduled batch in arrival order.
    async fn run(mut self) {
        loop {
            // Apply commands from the inbox to local state.
            while let Some(cmd) = self.prover.inbox.pop() {
                match cmd {
                    Command::Batch(batch) => self.process_batch(batch).await,
                    Command::Rollback(_target) => {
                        // TODO: add cancellation of in-flight proof requests
                    }
                }
                // Bail promptly once shutdown is signaled rather than draining the whole backlog:
                // a producer that runs ahead of the GPU leaves many queued batches the consumer no
                // longer needs, and proving them all would make teardown take minutes. The current
                // proof is awaited inline above, so this never cancels one mid-flight.
                if self.prover.shutdown.is_open() {
                    return;
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
        // Wait for tx artifacts on the batch before proving (composition needs them). Stay
        // shutdown-escapable so a wedged latch during teardown cannot hang the worker join.
        tokio::select! {
            biased;
            () = self.prover.shutdown.wait() => return,
            () = batch.wait_tx_artifacts_published() => {}
        }
        if batch.canceled() {
            return;
        }

        // The (checkpoint, block, batch-image) coordinate proves to the same per-batch receipt, so
        // a replay (including a flip reorg back onto this fork) reuses the cached one instead of
        // re-collecting the SMT proof and re-proving.
        let receipt = match batch.read_batch_receipt().resolve().await {
            Some(receipt) => receipt,
            None => {
                // Collect SMT proof at the version preceding the batch's checkpoint.
                let prev_version = batch.checkpoint().index().saturating_sub(1);
                let proof_bytes =
                    self.store.prove(&batch.resource_ids(), prev_version).expect("proof");

                // One pass: per-tx journal bytes (inputs) + receipt clones (proof composition).
                let (journals, receipts): (Vec<_>, Vec<_>) =
                    batch.tx_artifacts().map(|a| (B::journal_bytes(&a), (*a).clone())).unzip();

                // Encode the inputs for the proof.
                let covenant_id = self.config.covenant_id.as_bytes();
                let input_bytes = BatchInputs::encode(
                    BatchPins {
                        tx_image_id: self.backend.image_id(),
                        covenant_id: &covenant_id,
                        deposit_spk_hash: &self.config.deposit_spk_hash,
                        lane_key: &self.config.lane_key,
                    },
                    &proof_bytes,
                    batch.checkpoint().metadata(),
                    &journals,
                );

                // Compose the batch proof against those tx receipts.
                let receipt = self.backend.prove_batch(&input_bytes, receipts).await;

                // Wait for the receipt to be durable before publishing the artifact, so a crash
                // never leaves a consumed-but-uncached receipt.
                batch.write_batch_receipt(receipt.clone()).wait().await;
                receipt
            }
        };

        // Publish the receipt as the batch's artifact.
        batch.publish_artifact(Some(receipt));

        // Wait for this batch's commit so the next batch sees the committed prev_state, but stay
        // shutdown-escapable so a wedged latch during teardown cannot hang the join.
        tokio::select! {
            biased;
            () = self.prover.shutdown.wait() => {}
            () = batch.wait_committed() => {}
        }
    }
}
