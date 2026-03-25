use std::thread::spawn;

use tokio::runtime::Builder;
use vprogs_scheduling_scheduler::Processor;
use vprogs_storage_types::Store;

use crate::{Backend, api::Api};

/// Background worker that dispatches transaction proofs to a [`Backend`] concurrently.
pub(crate) struct Worker<S: Store, P: Processor<S>, B: Backend> {
    /// Shared prover state (inbox, shutdown).
    api: Api<S, P>,
    /// Backend used for proving.
    backend: B,
}

impl<S: Store, P: Processor<S, TransactionEffects = B::Receipt>, B: Backend> Worker<S, P, B> {
    pub(crate) fn spawn(api: Api<S, P>, backend: B) {
        let runtime = Builder::new_current_thread().enable_all().build().expect("runtime");
        spawn(move || runtime.block_on(Self { api, backend }.run()));
    }

    async fn run(self) {
        loop {
            // Dispatch all queued transactions for proving.
            while let Some((tx, tx_inputs)) = self.api.inbox.pop() {
                if tx.batch().upgrade().is_some_and(|b| !b.was_canceled()) {
                    let receipt = self.backend.prove_transaction(tx_inputs);
                    tokio::spawn(async move { tx.set_effects(receipt.await) });
                }
            }

            // Wait for a new submission or shutdown.
            tokio::select! {
                biased;
                () = self.api.shutdown.wait() => break,
                () = self.api.inbox.notified() => {}
            }
        }
    }
}
