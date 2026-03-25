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

use crate::{TransactionBackend, api::Api, input::Input};

/// A boxed proving future.
type ProvingFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Background worker that dispatches transaction proofs to a [`TransactionBackend`] concurrently.
pub(crate) struct Worker<P: Processor<S>, B: TransactionBackend, S: Store> {
    /// Api state with the prover handle.
    api: Api<P, B, S>,
    /// Transaction proofs currently pending.
    pending: FuturesUnordered<ProvingFuture>,
}

impl<P, B, S> Worker<P, B, S>
where
    P: Processor<S, TransactionEffects = B::Receipt>,
    B: TransactionBackend,
    S: Store,
{
    pub(crate) fn spawn(api: Api<P, B, S>) -> JoinHandle<()> {
        let runtime = Builder::new_current_thread().enable_all().build().expect("runtime");
        spawn(move || runtime.block_on(Self { api, pending: Default::default() }.run()))
    }

    async fn run(mut self) {
        while !self.api.is_sole_owner() || !self.pending.is_empty() {
            // Dispatch all queued transactions for proving.
            while let Some(Input { tx, mut tx_inputs }) = self.api.inbox.pop() {
                let canceled = tx.batch().upgrade().is_none_or(|b| b.was_canceled());
                if !canceled {
                    if let Ok(Inputs { .. }) = Inputs::decode(&mut tx_inputs[..]) {
                        let receipt = self.api.backend.prove_transaction(tx_inputs);
                        self.pending.push(Box::pin(async move {
                            tx.set_effects(receipt.await);
                        }));
                    };
                }
            }

            // Wait for either a new submission or a completed proof.
            tokio::select! {
                biased;
                () = self.api.inbox.notified() => {}
                Some(()) = self.pending.next(), if !self.pending.is_empty() => {}
            }
        }
    }
}
