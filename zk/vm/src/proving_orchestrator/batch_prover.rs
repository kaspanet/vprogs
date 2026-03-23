use std::thread::JoinHandle;

use tokio::runtime::Builder;
use vprogs_scheduling_scheduler::{Processor, ScheduledBatch};
use vprogs_storage_types::Store;
use vprogs_zk_abi::batch_processor::Inputs as BatchInputs;

use super::pending_batch::PendingBatch;
use crate::{AsyncQueue, Backend};

/// Worker that waits for completed batches, handles commit ordering, assembles batch
/// witnesses, and proves them.
pub(crate) struct BatchProver<P: Processor<S>, B: Backend, S: Store> {
    /// The proof backend used for proving batches.
    backend: B,
    /// The store used for reading SMT state at the pre-batch version.
    store: S,
    /// Transaction processor guest image ID.
    image_id: [u8; 32],
    /// Output queue for completed batch proof receipts.
    proof_queue: AsyncQueue<B::Receipt>,
    /// Completed batches from the transaction prover.
    inbox: AsyncQueue<PendingBatch<P, B, S>>,
    /// The most recently processed batch (for commit ordering).
    prev_batch: Option<ScheduledBatch<S, P>>,
}

impl<P: Processor<S>, B: Backend, S: Store> BatchProver<P, B, S> {
    pub(crate) fn spawn(
        backend: B,
        store: S,
        image_id: [u8; 32],
        proof_queue: AsyncQueue<B::Receipt>,
        inbox: AsyncQueue<PendingBatch<P, B, S>>,
    ) -> JoinHandle<()> {
        std::thread::spawn(move || {
            Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime")
                .block_on(
                    Self { backend, store, image_id, proof_queue, inbox, prev_batch: None }.run(),
                )
        })
    }

    async fn run(mut self) {
        while !self.inbox.is_singleton() {
            self.process_completed_batches().await;

            self.inbox.notified().await;
        }
    }

    /// Drains completed batches, waiting for commit ordering before assembling each.
    async fn process_completed_batches(&mut self) {
        while let Some(PendingBatch { batch, receipts, .. }) = self.inbox.pop() {
            // Wait for the previous batch to commit before reading the store.
            if let Some(ref prev) = self.prev_batch {
                prev.wait_committed().await;
            }

            // Skip canceled batches.
            if batch.was_canceled() {
                self.prev_batch = Some(batch);
                continue;
            }

            let (batch, receipt) = self.assemble_and_prove(batch, receipts).await;
            self.proof_queue.push(receipt);
            self.prev_batch = Some(batch);
        }
    }

    /// Assembles a batch witness from collected transaction receipts, proves the batch, and
    /// returns the raw batch receipt.
    async fn assemble_and_prove(
        &self,
        batch: ScheduledBatch<S, P>,
        receipts: Vec<Option<B::Receipt>>,
    ) -> (ScheduledBatch<S, P>, B::Receipt) {
        let prev_version = batch.checkpoint().index().saturating_sub(1);

        // Read resource IDs from the batch's state diffs (one per unique resource).
        // TODO: This allocation could be avoided if Tree::prove accepted an iterator instead
        //       of &[[u8; 32]].
        let resource_ids: Vec<[u8; 32]> =
            batch.state_diffs().iter().map(|diff| *diff.resource_id().as_bytes()).collect();
        let (proof_bytes, leaf_order) =
            self.store.prove(&resource_ids, prev_version).expect("SMT prove failed");

        // Consume receipt slots -- extract journals by ref, then take ownership.
        let tx_receipts: Vec<B::Receipt> =
            receipts.into_iter().map(|slot| slot.expect("missing receipt")).collect();
        let journals: Vec<Vec<u8>> = tx_receipts.iter().map(|r| B::journal_bytes(r)).collect();

        let input = BatchInputs::encode(&self.image_id, &proof_bytes, &leaf_order, &journals);
        let receipt = self.backend.prove_batch(&input, tx_receipts).await;
        (batch, receipt)
    }
}
