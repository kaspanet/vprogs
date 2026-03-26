use std::{collections::VecDeque, thread::spawn};

use tokio::runtime::Builder;
use vprogs_scheduling_scheduler::{Processor, ScheduledBatch};
use vprogs_storage_types::Store;
use vprogs_zk_abi::batch_processor::Inputs as BatchInputs;

use crate::{Backend, BatchProver, command::Command};

/// Background worker that assembles batch witnesses and proves them.
pub(crate) struct Worker<S: Store, P: Processor<S>, B: Backend> {
    /// Shared prover state (inbox, shutdown).
    prover: BatchProver<S, P>,
    /// Backend used for proving.
    backend: B,
    /// Store for reading SMT state proofs.
    store: S,
    /// Batches waiting to be proved, in scheduling order.
    pending: VecDeque<ScheduledBatch<S, P>>,
}

impl<S: Store, P, B: Backend> Worker<S, P, B>
where
    P: Processor<S, TransactionEffects = B::Receipt, BatchEffects = B::Receipt>,
{
    /// Spawns the worker on a new thread with a single-threaded tokio runtime.
    pub(crate) fn spawn(prover: BatchProver<S, P>, backend: B, store: S) {
        let this = Self { prover, backend, store, pending: VecDeque::new() };
        let runtime = Builder::new_current_thread().enable_all().build().expect("runtime");
        spawn(move || runtime.block_on(this.run()));
    }

    /// Main loop: drains commands from the inbox, processes pending batches, and waits for new
    /// work or shutdown.
    async fn run(mut self) {
        loop {
            // Apply commands from the inbox to local state.
            while let Some(cmd) = self.prover.inbox.pop() {
                match cmd {
                    Command::Batch(batch) => self.pending.push_back(batch),
                    Command::Rollback(target) => {
                        self.pending.retain(|b| b.checkpoint().index() <= target);
                    }
                }
            }

            // Register notification before popping so we don't race with new commands arriving.
            let inbox_updated = self.prover.inbox.notified();

            // Process the next batch or wait for a new command / shutdown.
            match self.pending.pop_front() {
                Some(batch) => {
                    // Release the inbox borrow so `process_batch` can take `&mut self`.
                    drop(inbox_updated);

                    // Process the next batch in the schedule.
                    self.process_batch(batch).await;
                }
                None => tokio::select! {
                    biased;
                    () = self.prover.shutdown.wait() => break,
                    () = inbox_updated => {}
                },
            }
        }
    }

    /// Processes a single batch through the proving pipeline.
    async fn process_batch(&mut self, batch: ScheduledBatch<S, P>) {
        // Wait for all transaction effects to be published.
        batch.wait_tx_effects_ready().await;
        if batch.was_canceled() {
            return;
        }

        // Collect receipts from batch transactions and prove.
        let receipts = batch.txs().iter().map(|tx| (*tx.effects()).clone()).collect();
        let receipt = self.assemble_and_prove(&batch, receipts).await;

        // Publish the batch proof as batch-level effects.
        batch.set_effects(Some(receipt));

        // Wait for this batch to commit before returning to the main loop. This guarantees the
        // next batch sees committed SMT state when it reads proofs.
        batch.wait_committed().await;
    }

    /// Assembles the batch witness from transaction receipts and proves it.
    async fn assemble_and_prove(
        &self,
        batch: &ScheduledBatch<S, P>,
        receipts: Vec<B::Receipt>,
    ) -> B::Receipt {
        let prev_version = batch.checkpoint().index().saturating_sub(1);

        let resource_ids: Vec<[u8; 32]> =
            batch.state_diffs().iter().map(|diff| *diff.resource_id().as_bytes()).collect();
        let (proof_bytes, leaf_order) =
            self.store.prove(&resource_ids, prev_version).expect("SMT prove failed");

        let journals: Vec<Vec<u8>> = receipts.iter().map(|r| B::journal_bytes(r)).collect();

        let input =
            BatchInputs::encode(self.backend.image_id(), &proof_bytes, &leaf_order, &journals);
        self.backend.prove_batch(&input, receipts).await
    }
}
