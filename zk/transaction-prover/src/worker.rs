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

use crate::{Backend, api::Api};

/// Background worker that dispatches transaction proofs to a [`Backend`] concurrently.
pub(crate) struct Worker<S: Store, P: Processor<S>, B: Backend> {
    api: Api<S, P>,
    backend: B,
    pending: FuturesUnordered<Pin<Box<dyn Future<Output = ()> + Send>>>,
}

impl<S: Store, P: Processor<S, TransactionEffects = B::Receipt>, B: Backend> Worker<S, P, B> {
    pub(crate) fn spawn(api: Api<S, P>, backend: B) -> JoinHandle<()> {
        let runtime = Builder::new_current_thread().enable_all().build().expect("runtime");
        spawn(move || runtime.block_on(Self { api, backend, pending: Default::default() }.run()))
    }

    async fn run(mut self) {
        loop {
            // Dispatch all queued transactions for proving.
            while let Some((tx, mut tx_inputs)) = self.api.inbox.pop() {
                let canceled = tx.batch().upgrade().is_none_or(|b| b.was_canceled());
                if !canceled {
                    if let Ok(Inputs { .. }) = Inputs::decode(&mut tx_inputs[..]) {
                        let receipt = self.backend.prove_transaction(tx_inputs);
                        self.pending.push(Box::pin(async move {
                            tx.set_effects(receipt.await);
                        }));
                    };
                }
            }

            // Wait for a new submission, a completed proof, or shutdown.
            tokio::select! {
                biased;
                () = self.api.shutdown.wait() => break,
                () = self.api.inbox.notified() => {}
                Some(()) = self.pending.next(), if !self.pending.is_empty() => {}
            }
        }
    }
}
