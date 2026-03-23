use std::{collections::HashMap, thread::JoinHandle};

use futures::stream::{FuturesUnordered, StreamExt};
use tokio::runtime::Builder;
use vprogs_scheduling_scheduler::Processor;
use vprogs_storage_types::Store;
use vprogs_zk_abi::transaction_processor::Inputs;

use super::{batch::ProvingBatch, completed_proof::CompletedTxProof, task::ProofTask};
use crate::{AsyncQueue, ProofProvider, proof_provider::BoxFuture};

/// Worker that dispatches transaction proofs to a [`ProofProvider`] concurrently, tracks
/// per-batch receipt accumulation, and pushes completed batches to the batch prover.
pub(crate) struct TransactionProver<P: Processor<S>, Provider: ProofProvider, S: Store> {
    /// Proof tasks submitted by execution workers.
    task_queue: AsyncQueue<ProofTask<P, S>>,
    /// The proof provider used for proving transactions.
    provider: Provider,
    /// Completed batches forwarded to the batch prover.
    completed_queue: AsyncQueue<ProvingBatch<P, Provider, S>>,
    /// Receipts accumulating per batch (keyed by batch index).
    pending: HashMap<u64, ProvingBatch<P, Provider, S>>,
    /// Transaction proofs currently in flight.
    in_flight: FuturesUnordered<BoxFuture<CompletedTxProof<P, Provider, S>>>,
}

impl<P: Processor<S>, Provider: ProofProvider, S: Store> TransactionProver<P, Provider, S> {
    pub(crate) fn spawn(
        task_queue: AsyncQueue<ProofTask<P, S>>,
        provider: Provider,
        completed_queue: AsyncQueue<ProvingBatch<P, Provider, S>>,
    ) -> JoinHandle<()> {
        std::thread::spawn(move || {
            Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime")
                .block_on(
                    Self {
                        task_queue,
                        provider,
                        completed_queue,
                        pending: HashMap::new(),
                        in_flight: FuturesUnordered::new(),
                    }
                    .run(),
                )
        })
    }

    async fn run(mut self) {
        while !self.should_exit() {
            self.drain_task_queue();

            tokio::select! {
                biased;
                () = self.task_queue.notified() => {}
                Some(proof) = self.in_flight.next(), if !self.in_flight.is_empty() => {
                    self.record_receipt(proof);
                }
            }
        }
    }

    /// Drains all queued tasks and submits them to the proof provider.
    fn drain_task_queue(&mut self) {
        while let Some(mut task) = self.task_queue.pop() {
            if task.batch.was_canceled() {
                continue;
            }

            let Ok(Inputs { tx_index, .. }) = Inputs::decode(&mut task.input_bytes[..]) else {
                continue;
            };

            let proof_future = self.provider.prove_transaction(task.input_bytes);
            let batch = task.batch;

            self.in_flight.push(Box::pin(async move {
                CompletedTxProof { batch, tx_index, receipt: proof_future.await }
            }));
        }
    }

    /// Records a completed transaction receipt and forwards the batch when all receipts are
    /// collected.
    fn record_receipt(&mut self, proof: CompletedTxProof<P, Provider, S>) {
        let batch_index = proof.batch.checkpoint().index();
        let tx_count = proof.batch.txs().len() as u32;

        let batch = self.pending.entry(batch_index).or_insert_with(|| ProvingBatch {
            batch: proof.batch,
            receipts: vec![None; tx_count as usize],
            received: 0,
        });

        batch.receipts[proof.tx_index as usize] = Some(proof.receipt);
        batch.received += 1;

        // All transactions proven - forward to batch prover.
        if tx_count > 0 && batch.received >= tx_count {
            let batch = self.pending.remove(&batch_index).unwrap();
            self.completed_queue.push(batch);
        }
    }

    /// Returns true when all external references are dropped and no work remains.
    fn should_exit(&self) -> bool {
        self.task_queue.is_singleton() && self.in_flight.is_empty()
    }
}
