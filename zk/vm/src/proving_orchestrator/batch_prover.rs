use std::thread::JoinHandle;

use tokio::runtime::Builder;
use vprogs_scheduling_scheduler::{Processor, ScheduledBatch};
use vprogs_storage_types::Store;
use vprogs_zk_abi::batch_processor::Inputs as BatchInputs;

use super::batch::ProvingBatch;
use crate::{AsyncQueue, ProofProvider};

/// Worker that waits for completed batches, handles commit ordering, assembles batch
/// witnesses, and proves them.
pub(crate) struct BatchProver<P: Processor<S>, Provider: ProofProvider, S: Store> {
    /// The proof provider used for proving batches.
    provider: Provider,
    /// The store used for reading SMT state at the pre-batch version.
    store: S,
    /// Transaction processor guest image ID.
    image_id: [u8; 32],
    /// Output queue for completed batch proof receipts.
    proof_queue: AsyncQueue<Provider::Receipt>,
    /// Completed batches from the transaction prover.
    completed_queue: AsyncQueue<ProvingBatch<P, Provider, S>>,
    /// The most recently processed batch (for commit ordering).
    prev_batch: Option<ScheduledBatch<S, P>>,
}

impl<P: Processor<S>, Provider: ProofProvider, S: Store> BatchProver<P, Provider, S> {
    pub(crate) fn spawn(
        provider: Provider,
        store: S,
        image_id: [u8; 32],
        proof_queue: AsyncQueue<Provider::Receipt>,
        completed_queue: AsyncQueue<ProvingBatch<P, Provider, S>>,
    ) -> JoinHandle<()> {
        std::thread::spawn(move || {
            Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime")
                .block_on(
                    Self {
                        provider,
                        store,
                        image_id,
                        proof_queue,
                        completed_queue,
                        prev_batch: None,
                    }
                    .run(),
                )
        })
    }

    async fn run(mut self) {
        while !self.completed_queue.is_singleton() {
            self.process_completed_batches().await;

            self.completed_queue.notified().await;
        }
    }

    /// Drains completed batches, waiting for commit ordering before assembling each.
    async fn process_completed_batches(&mut self) {
        while let Some(completed) = self.completed_queue.pop() {
            // Wait for the previous batch to commit before reading the store.
            if let Some(ref prev) = self.prev_batch {
                prev.wait_committed().await;
            }

            // Skip canceled batches.
            if !completed.batch.was_canceled() {
                let receipt = self.assemble_and_prove(&completed).await;
                self.proof_queue.push(receipt);
            }

            self.prev_batch = Some(completed.batch);
        }
    }

    /// Assembles a batch witness from collected transaction receipts, proves the batch, and
    /// returns the raw batch receipt.
    async fn assemble_and_prove(
        &self,
        proving_batch: &ProvingBatch<P, Provider, S>,
    ) -> Provider::Receipt {
        let prev_version = proving_batch.batch.checkpoint().index().saturating_sub(1);

        // Read resource IDs from the batch's state diffs (one per unique resource).
        // TODO: This allocation could be avoided if Tree::prove accepted an iterator instead
        //       of &[[u8; 32]].
        let resource_ids: Vec<[u8; 32]> = proving_batch
            .batch
            .state_diffs()
            .iter()
            .map(|diff| *diff.resource_id().as_bytes())
            .collect();
        let (proof_bytes, leaf_order) =
            self.store.prove(&resource_ids, prev_version).expect("SMT prove failed");

        // Extract journals and owned receipts in a single pass over the receipt slots.
        let mut journals = Vec::with_capacity(proving_batch.receipts.len());
        let mut tx_receipts = Vec::with_capacity(proving_batch.receipts.len());
        for slot in &proving_batch.receipts {
            let receipt = slot.as_ref().expect("missing receipt");
            journals.push(Provider::journal_bytes(receipt));
            tx_receipts.push(receipt.clone());
        }

        self.provider
            .prove_batch(
                BatchInputs::encode(&self.image_id, &proof_bytes, &leaf_order, &journals),
                tx_receipts,
            )
            .await
    }
}
