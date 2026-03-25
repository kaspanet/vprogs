use std::{
    sync::Arc,
    thread::{JoinHandle, spawn},
};

use tokio::runtime::Builder;
use vprogs_scheduling_scheduler::{Processor, ScheduledBatch};
use vprogs_storage_types::Store;
use vprogs_zk_abi::batch_processor::Inputs as BatchInputs;

use crate::{BatchBackend, api::Api};

/// Background worker that waits for batch effects to be ready, assembles batch witnesses with
/// SMT proofs, and proves them.
pub(crate) struct Worker<P: Processor<S>, BB: BatchBackend, S: Store> {
    /// Shared API (store, results queue).
    api: Api<P, BB, S>,
    /// The most recently processed batch (for commit ordering).
    prev_batch: Option<ScheduledBatch<S, P>>,
}

impl<P, BB, S> Worker<P, BB, S>
where
    P: Processor<S, TransactionEffects = BB::Receipt>,
    BB: BatchBackend,
    S: Store,
{
    pub(crate) fn spawn(api: Api<P, BB, S>) -> JoinHandle<()> {
        let runtime = Builder::new_current_thread().enable_all().build().expect("runtime");
        spawn(move || runtime.block_on(Self { api, prev_batch: None }.run()))
    }

    async fn run(mut self) {
        loop {
            // Drain all batches queued for proving.
            while let Some(batch) = self.api.inbox.pop() {
                self.process_batch(batch).await;
            }

            // Exit once all external Api references are dropped and no batches remain.
            if Arc::strong_count(&self.api.0) == 1 {
                break;
            }

            self.api.inbox.notified().await;
        }
    }

    /// Waits for effects, assembles the batch witness, and proves it.
    async fn process_batch(&mut self, batch: ScheduledBatch<S, P>) {
        // Wait for all transaction receipts to be published.
        batch.wait_effects_ready().await;

        // Wait for the previous batch to commit before reading SMT state.
        if let Some(ref prev) = self.prev_batch {
            prev.wait_committed().await;
        }

        // Skip cancelled batches but still track them for ordering.
        if batch.was_canceled() {
            self.prev_batch = Some(batch);
            return;
        }

        // Read receipts directly from batch transactions.
        let receipts: Vec<BB::Receipt> =
            batch.txs().iter().map(|tx| (*tx.effects()).clone()).collect();
        let (scheduled, receipt) = self.assemble_and_prove(batch, receipts).await;
        self.api.outbox.push(receipt);
        self.prev_batch = Some(scheduled);
    }

    /// Assembles a batch witness from collected transaction receipts, proves the batch, and
    /// returns the scheduled batch handle and raw batch receipt.
    async fn assemble_and_prove(
        &self,
        batch: ScheduledBatch<S, P>,
        receipts: Vec<BB::Receipt>,
    ) -> (ScheduledBatch<S, P>, BB::Receipt) {
        let prev_version = batch.checkpoint().index().saturating_sub(1);

        // Read resource IDs from the batch's state diffs (one per unique resource).
        let resource_ids: Vec<[u8; 32]> =
            batch.state_diffs().iter().map(|diff| *diff.resource_id().as_bytes()).collect();
        let (proof_bytes, leaf_order) =
            self.api.store.prove(&resource_ids, prev_version).expect("SMT prove failed");

        let journals: Vec<Vec<u8>> = receipts.iter().map(|r| BB::journal_bytes(r)).collect();

        let input =
            BatchInputs::encode(self.api.backend.image_id(), &proof_bytes, &leaf_order, &journals);
        let receipt = self.api.backend.prove_batch(&input, receipts).await;
        (batch, receipt)
    }
}
