use std::thread::{JoinHandle, spawn};

use tokio::runtime::Builder;
use vprogs_l1_types::ChainBlockMetadata;
use vprogs_scheduling_scheduler::Processor;
use vprogs_storage_types::Store;

use crate::{Backend, TransactionProver};

/// Background worker that proves queued transactions through a [`Backend`], one at a time.
pub(crate) struct Worker<S: Store, P: Processor<S>, B: Backend> {
    /// Shared prover state (inbox, shutdown).
    prover: TransactionProver<S, P>,
    /// Backend used for proving.
    backend: B,
}

impl<S, P, B: Backend> Worker<S, P, B>
where
    S: Store,
    P: Processor<S, TransactionArtifact = B::Receipt, BatchMetadata = ChainBlockMetadata>,
{
    /// Spawns the worker on a new thread with a single-threaded tokio runtime and returns its join
    /// handle. The prover joins this on shutdown so the worker's GPU prover is torn down (its risc0
    /// CUDA context released) before the process exits.
    pub(crate) fn spawn(prover: TransactionProver<S, P>, backend: B) -> JoinHandle<()> {
        let runtime = Builder::new_current_thread().enable_all().build().expect("runtime");
        spawn(move || runtime.block_on(Self { prover, backend }.run()))
    }

    /// Main loop: drains the inbox, dispatches proofs, and waits for new work or shutdown.
    async fn run(self) {
        loop {
            // Register notification before draining so we don't miss pushes.
            let notified = self.prover.inbox.notified();

            // Prove each queued transaction in turn.
            while let Some((tx, tx_inputs)) = self.prover.inbox.pop() {
                // Hold the batch alive for the iteration: the tx's receipt lookups reach storage
                // through it, and dropping it would also drop the only path to the cache.
                let Some(_batch) = tx.batch().upgrade().filter(|b| !b.canceled()) else {
                    // Canceled or dropped batch: advance the counter without proving.
                    tx.publish_artifact(None);
                    continue;
                };

                // Coordinate this receipt with the proof-receipt cache: the same (checkpoint,
                // block, image, merge index) coordinate proves to the same receipt, so a replay
                // (including a flip reorg back onto this fork) reuses it instead of re-proving.
                if let Some(receipt) = tx.read_tx_receipt().resolve().await {
                    tx.publish_artifact(Some(receipt));
                    continue;
                }

                let receipt = self.backend.prove_transaction(tx_inputs).await;

                // Wait for the receipt to be durable before publishing the artifact, so a crash
                // never leaves a consumed-but-uncached receipt.
                tx.write_tx_receipt(receipt.clone()).wait().await;
                tx.publish_artifact(Some(receipt));
            }

            // Wait for a new submission or shutdown.
            tokio::select! {
                biased;
                () = self.prover.shutdown.wait() => break,
                () = notified => {}
            }
        }
    }
}
