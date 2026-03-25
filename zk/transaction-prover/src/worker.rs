use std::thread::spawn;

use tokio::runtime::Builder;
use vprogs_scheduling_scheduler::Processor;
use vprogs_storage_types::Store;

use crate::{Backend, TransactionProver};

/// Background worker that dispatches transaction proofs to a [`Backend`] concurrently.
pub(crate) struct Worker<S: Store, P: Processor<S>, B: Backend> {
    /// Shared prover state (inbox, shutdown).
    prover: TransactionProver<S, P>,
    /// Backend used for proving.
    backend: B,
}

impl<S: Store, P: Processor<S, TransactionEffects = B::Receipt>, B: Backend> Worker<S, P, B> {
    /// Spawns the worker on a new thread with a single-threaded tokio runtime.
    pub(crate) fn spawn(prover: TransactionProver<S, P>, backend: B) {
        let runtime = Builder::new_current_thread().enable_all().build().expect("runtime");
        spawn(move || runtime.block_on(Self { prover, backend }.run()));
    }

    /// Main loop: drains the inbox, dispatches proofs, and waits for new work or shutdown.
    async fn run(self) {
        loop {
            // Dispatch all queued transactions for proving.
            while let Some((tx, tx_inputs)) = self.prover.inbox.pop() {
                if tx.batch().upgrade().is_some_and(|b| !b.was_canceled()) {
                    let receipt = self.backend.prove_transaction(tx_inputs);
                    tokio::spawn(async move { tx.set_effects(receipt.await) });
                }
            }

            // Wait for a new submission or shutdown.
            tokio::select! {
                biased;
                () = self.prover.shutdown.wait() => break,
                () = self.prover.inbox.notified() => {}
            }
        }
    }
}
