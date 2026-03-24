use std::{pin::Pin, thread::JoinHandle};

use futures::{
    Future,
    stream::{FuturesUnordered, StreamExt},
};
use tokio::runtime::Builder;
use vprogs_core_atomics::AsyncQueue;
use vprogs_scheduling_scheduler::Processor;
use vprogs_storage_types::Store;
use vprogs_zk_abi::transaction_processor::Inputs;

use crate::{
    TransactionBackend, pending_transaction::PendingTransaction,
    proved_transaction::ProvedTransaction,
};

/// Background worker that dispatches transaction proofs to a [`TransactionBackend`] concurrently
/// and forwards each completed receipt as a [`ProvedTransaction`].
pub(crate) struct TransactionProverWorker<P: Processor<S>, B: TransactionBackend, S: Store> {
    /// The proof backend used for proving transactions.
    backend: B,
    /// Proof tasks submitted by execution workers.
    inbox: AsyncQueue<PendingTransaction<P, S>>,
    /// Proved transaction receipts forwarded to the results consumer.
    outbox: AsyncQueue<ProvedTransaction<P, B, S>>,
    /// Transaction proofs currently pending.
    pending: FuturesUnordered<ProvingFuture<P, B, S>>,
}

impl<P: Processor<S>, B: TransactionBackend, S: Store> TransactionProverWorker<P, B, S> {
    pub(crate) fn new(
        backend: B,
        inbox: AsyncQueue<PendingTransaction<P, S>>,
        outbox: AsyncQueue<ProvedTransaction<P, B, S>>,
    ) -> Self {
        Self { backend, inbox, outbox, pending: FuturesUnordered::new() }
    }

    pub(crate) fn spawn(self) -> JoinHandle<()> {
        std::thread::spawn(move || {
            Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime")
                .block_on(self.run())
        })
    }

    async fn run(mut self) {
        while !self.inbox.is_singleton() || !self.pending.is_empty() {
            // Dispatch all queued transactions for proving.
            while let Some(PendingTransaction { batch, mut tx_inputs }) = self.inbox.pop() {
                if !batch.was_canceled() {
                    if let Ok(Inputs { tx_index, .. }) = Inputs::decode(&mut tx_inputs[..]) {
                        let receipt = self.backend.prove_transaction(tx_inputs);
                        self.pending.push(Box::pin(async move {
                            ProvedTransaction { batch, index: tx_index, receipt: receipt.await }
                        }));
                    };
                }
            }

            // Wait for either a new submission or a completed proof.
            tokio::select! {
                biased;
                () = self.inbox.notified() => {}
                Some(proved) = self.pending.next(), if !self.pending.is_empty() => {
                    self.outbox.push(proved);
                }
            }
        }
    }
}

/// A boxed proving future that resolves to a [`ProvedTransaction`].
type ProvingFuture<P, B, S> = Pin<Box<dyn Future<Output = ProvedTransaction<P, B, S>> + Send>>;
