use std::thread::spawn;

use tokio::runtime::Builder;
use vprogs_core_atomics::AsyncQueue;
use vprogs_scheduling_scheduler::{Processor, ScheduledBatch};
use vprogs_storage_types::Store;
use vprogs_zk_abi::batch_processor::Inputs as BatchInputs;

use crate::{Backend, BatchProver};

/// Background worker that assembles batch witnesses and proves them.
pub(crate) struct Worker<S: Store, P: Processor<S>, B: Backend> {
    /// Shared prover state (inbox, shutdown).
    prover: BatchProver<S, P>,
    /// Backend used for proving.
    backend: B,
    /// Store for reading SMT state proofs.
    store: S,
    /// Batch proof receipts.
    outbox: AsyncQueue<B::Receipt>,
    /// Previous batch, tracked for commit ordering.
    prev_batch: Option<ScheduledBatch<S, P>>,
}

impl<S: Store, P: Processor<S, TransactionEffects = B::Receipt>, B: Backend> Worker<S, P, B> {
    /// Spawns the worker on a new thread with a single-threaded tokio runtime.
    pub(crate) fn spawn(
        prover: BatchProver<S, P>,
        backend: B,
        store: S,
        outbox: AsyncQueue<B::Receipt>,
    ) {
        let runtime = Builder::new_current_thread().enable_all().build().expect("runtime");
        spawn(move || {
            runtime.block_on(Self { prover, backend, store, outbox, prev_batch: None }.run())
        });
    }

    /// Main loop: drains the inbox, processes batches, and waits for new work or shutdown.
    async fn run(mut self) {
        loop {
            // Drain all batches queued for proving.
            while let Some(batch) = self.prover.inbox.pop() {
                self.process_batch(batch).await;
            }

            // Wait for a new batch or shutdown.
            tokio::select! {
                biased;
                () = self.prover.shutdown.wait() => break,
                () = self.prover.inbox.notified() => {}
            }
        }
    }

    /// Processes a single batch through the proving pipeline.
    async fn process_batch(&mut self, batch: ScheduledBatch<S, P>) {
        // Wait for all transaction effects to be published.
        batch.wait_effects_ready().await;

        // Skip canceled batches but still track them for ordering.
        if batch.was_canceled() {
            self.prev_batch = Some(batch);
            return;
        }

        // Wait for the previous batch to commit before reading SMT state.
        if let Some(ref prev) = self.prev_batch {
            prev.wait_committed().await;
        }

        // Re-check after waiting - batch may have been canceled in the meantime.
        if batch.was_canceled() {
            self.prev_batch = Some(batch);
            return;
        }

        // Collect receipts from batch transactions and prove.
        let receipts = batch.txs().iter().map(|tx| (*tx.effects()).clone()).collect();
        let (scheduled, receipt) = self.assemble_and_prove(batch, receipts).await;
        self.outbox.push(receipt);
        self.prev_batch = Some(scheduled);
    }

    /// Assembles the batch witness from transaction receipts and proves it.
    async fn assemble_and_prove(
        &self,
        batch: ScheduledBatch<S, P>,
        receipts: Vec<B::Receipt>,
    ) -> (ScheduledBatch<S, P>, B::Receipt) {
        let prev_version = batch.checkpoint().index().saturating_sub(1);

        let resource_ids: Vec<[u8; 32]> =
            batch.state_diffs().iter().map(|diff| *diff.resource_id().as_bytes()).collect();
        let (proof_bytes, leaf_order) =
            self.store.prove(&resource_ids, prev_version).expect("SMT prove failed");

        let journals: Vec<Vec<u8>> = receipts.iter().map(|r| B::journal_bytes(r)).collect();

        let input =
            BatchInputs::encode(self.backend.image_id(), &proof_bytes, &leaf_order, &journals);
        let receipt = self.backend.prove_batch(&input, receipts).await;
        (batch, receipt)
    }
}
