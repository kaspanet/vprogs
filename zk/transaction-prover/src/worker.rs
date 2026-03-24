use std::{
    pin::Pin,
    thread::{JoinHandle, spawn},
};

use futures::{
    Future,
    stream::{FuturesUnordered, StreamExt},
};
use tokio::runtime::Builder;
use vprogs_scheduling_scheduler::Processor;
use vprogs_storage_types::Store;
use vprogs_zk_abi::transaction_processor::Inputs;

use crate::{
    TransactionBackend, pending_transaction::PendingTransaction,
    proved_transaction::ProvedTransaction, worker_api::WorkerApi,
};

/// A boxed proving future that resolves to a [`ProvedTransaction`].
type ProvingFuture<P, B, S> = Pin<Box<dyn Future<Output = ProvedTransaction<P, B, S>> + Send>>;

/// Background worker that dispatches transaction proofs to a [`TransactionBackend`] concurrently
/// and forwards each completed receipt as a [`ProvedTransaction`].
pub(crate) struct Worker<P: Processor<S>, B: TransactionBackend, S: Store> {
    /// WorkerApi state with the prover handle.
    api: WorkerApi<P, B, S>,
    /// Transaction proofs currently pending.
    pending: FuturesUnordered<ProvingFuture<P, B, S>>,
}

impl<P: Processor<S>, B: TransactionBackend, S: Store> Worker<P, B, S> {
    pub(crate) fn spawn(api: WorkerApi<P, B, S>) -> JoinHandle<()> {
        let runtime = Builder::new_current_thread().enable_all().build().expect("runtime");
        spawn(move || runtime.block_on(Self { api, pending: Default::default() }.run()))
    }

    async fn run(mut self) {
        while !self.api.is_sole_owner() || !self.pending.is_empty() {
            // Dispatch all queued transactions for proving.
            while let Some(PendingTransaction { batch, mut tx_inputs }) = self.api.inbox.pop() {
                if !batch.was_canceled() {
                    if let Ok(Inputs { tx_index, .. }) = Inputs::decode(&mut tx_inputs[..]) {
                        let receipt = self.api.backend.prove_transaction(tx_inputs);
                        self.pending.push(Box::pin(async move {
                            ProvedTransaction { batch, index: tx_index, receipt: receipt.await }
                        }));
                    };
                }
            }

            // Wait for either a new submission or a completed proof.
            tokio::select! {
                biased;
                () = self.api.inbox.notified() => {}
                Some(proved) = self.pending.next(), if !self.pending.is_empty() => {
                    self.api.outbox.push(proved);
                }
            }
        }
    }
}
