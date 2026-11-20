use std::thread::{JoinHandle, spawn};

use tokio::runtime::Builder;
use vprogs_l1_types::ChainBlockMetadata;
use vprogs_scheduling_scheduler::{Processor, ScheduledBatch};
use vprogs_storage_types::Store;
use vprogs_zk_abi::batch_processor::{BatchPins, BatchTransition, Inputs as BatchInputs};
use zerocopy::FromBytes;

use crate::{Backend, BatchProver, BatchProverConfig, command::Command};

/// Background worker that drains the prover inbox and produces one ZK receipt per scheduled batch.
pub(crate) struct Worker<S: Store, P: Processor<S>, B: Backend> {
    /// Shared prover state (inbox, shutdown).
    prover: BatchProver<S, P>,
    /// Backend used for proving.
    backend: B,
    /// Store for reading SMT state proofs.
    store: S,
    /// Static config (lane key, covenant id).
    config: BatchProverConfig,
}

impl<S: Store, P, B: Backend> Worker<S, P, B>
where
    P: Processor<
            S,
            TransactionArtifact = B::Receipt,
            BatchArtifact = B::Receipt,
            BatchMetadata = ChainBlockMetadata,
        >,
{
    /// Spawns the worker on a new thread with a single-threaded tokio runtime and returns its join
    /// handle. The prover joins this on shutdown so the worker's GPU prover is torn down (its risc0
    /// CUDA context released) before the process exits.
    pub(crate) fn spawn(
        prover: BatchProver<S, P>,
        backend: B,
        store: S,
        config: BatchProverConfig,
    ) -> JoinHandle<()> {
        let this = Self { prover, backend, store, config };
        let runtime = Builder::new_current_thread().enable_all().build().expect("runtime");
        spawn(move || {
            runtime.block_on(this.run());
            // A timed-out prove attempt's blocking task is abandoned, not joined: dropping the
            // runtime would wait for it and the prover's shutdown join with it, so shut the
            // runtime down in the background instead.
            runtime.shutdown_background();
        })
    }

    /// Main loop: drain commands and prove each scheduled batch in arrival order.
    async fn run(mut self) {
        loop {
            // Apply commands from the inbox to local state.
            while let Some(cmd) = self.prover.inbox.pop() {
                match cmd {
                    Command::Batch(batch) => self.process_batch(batch).await,
                    Command::Rollback(_target) => {
                        // TODO: add cancellation of in-flight proof requests
                    }
                }
                // Bail promptly once shutdown is signaled rather than draining the whole backlog:
                // a producer that runs ahead of the GPU leaves many queued batches the consumer no
                // longer needs, and proving them all would make teardown take minutes. The current
                // proof is awaited inline above, so this never cancels one mid-flight.
                if self.prover.shutdown.is_open() {
                    return;
                }
            }

            tokio::select! {
                biased;
                () = self.prover.shutdown.wait() => break,
                () = self.prover.inbox.notified() => {}
            }
        }
    }

    /// Publishes the batch's artifact: a proof receipt for a non-empty batch, or an empty artifact
    /// (no proving) when the batch carries no txs.
    async fn process_batch(&mut self, batch: ScheduledBatch<S, P>) {
        // Wait for tx artifacts on the batch before proving (composition needs them). Stay
        // shutdown-escapable so a wedged latch during teardown cannot hang the worker join.
        tokio::select! {
            biased;
            () = self.prover.shutdown.wait() => return,
            () = batch.wait_tx_artifacts_published() => {}
        }
        if batch.canceled() {
            return;
        }

        // Skip proving an empty batch (a chain block with no lane txs): it advances no L2 state and
        // the aggregate prover composes only non-empty batch journals (filtering on this same
        // `txs().is_empty()` predicate), so its receipt is never read. Proving every empty backfill
        // block in order is O(chain-length) dead work that stalls a deep-seeded node before it can
        // form its first bundle. Publish an empty artifact so the aggregate prover's ready-prefix
        // wait resolves, then wait for the commit so the next batch sees the committed prev_state.
        if batch.txs().is_empty() {
            batch.publish_artifact(None);
            batch.wait_committed().await;
            return;
        }

        // The (checkpoint, block, batch-image) coordinate normally proves to the same per-batch
        // receipt, so a replay (including a flip reorg back onto this fork) reuses the cached one
        // instead of re-collecting the SMT proof and re-proving. The coordinate does not name
        // where the batch enters, though: a block returning under its batch id after a rollback
        // or a re-root can find a receipt proved from another state root or lane tip. Publishing
        // that one keeps the batch off the covenant tip for good (the re-root requested to
        // rebuild it gets the same receipt back) and trips the root compare below when it also
        // exits elsewhere, so a cached receipt is reused only while it enters where this batch
        // does.
        let prev_version = batch.checkpoint().index().saturating_sub(1);
        let cached = batch.read_batch_receipt().resolve().await;
        let mismatch = cached.as_ref().and_then(|r| self.entry_mismatch(&batch, prev_version, r));

        let receipt = match cached.filter(|_| mismatch.is_none()) {
            Some(receipt) => receipt,
            None => {
                // Collect SMT proof at the version preceding the batch's checkpoint.
                let proof_bytes =
                    self.store.prove(&batch.resource_ids(), prev_version).expect("proof");

                // A rollback that reached below the version the entry was checked and the proof
                // collected at can change what both read: the check then misreads a matching
                // receipt, and the proof roots the batch in another lineage than its tx
                // receipts, which the guest can reject on every retry. A rollback cancels its
                // batches before it moves the canonical chain, so reads that finished without
                // such a rollback saw the batch's own lineage. The bound is the version read
                // at, not the batch's parent: below it a sibling fork can return under an id
                // in between. A rollback to exactly that version (a re-root to the boundary
                // when the batch has the next id) cancels the batch but leaves what it read
                // intact, and its proof still serves the block's return.
                if batch.rolled_back_below(prev_version) {
                    return;
                }
                if let Some(field) = mismatch {
                    log::warn!(
                        "batch-prover: cached receipt for batch {} does not enter where the \
                         batch does ({field}); proving it afresh",
                        batch.checkpoint().index(),
                    );
                }

                // One pass: per-tx journal bytes (inputs) + receipt clones (proof composition).
                let (journals, receipts): (Vec<_>, Vec<_>) =
                    batch.tx_artifacts().map(|a| (B::journal_bytes(&a), (*a).clone())).unzip();

                // Encode the inputs for the proof.
                let covenant_id = self.config.covenant_id.as_bytes();
                let input_bytes = BatchInputs::encode(
                    BatchPins {
                        tx_image_id: self.backend.image_id(),
                        covenant_id: &covenant_id,
                        deposit_spk_hash: &self.config.deposit_spk_hash,
                        lane_key: &self.config.lane_key,
                    },
                    &proof_bytes,
                    batch.checkpoint().metadata(),
                    &journals,
                );

                // One attempt per iteration, awaited under the shutdown latch: a persistent
                // failure retries with the worker escapable, so the prover's shutdown join never
                // waits on an infinite retry. No attempt cap and no exhaustion panic; a
                // deterministic failure retries identically and loudly until an operator's
                // keepalive flags the stall. Each attempt costs a real prove, so the loop is
                // naturally paced; a "Guest panicked" error retries like any other failure.
                let mut attempt = 1u32;
                let receipt = loop {
                    let proved = tokio::select! {
                        biased;
                        () = self.prover.shutdown.wait() => return,
                        proved = self.backend.prove_batch(&input_bytes, receipts.clone()) => {
                            proved
                        }
                    };
                    match proved {
                        Ok(receipt) => break receipt,
                        Err(err) => {
                            log::warn!(
                                "batch-prover: prove attempt {attempt} failed ({err}); retrying"
                            );
                            attempt += 1;
                        }
                    }
                };

                // Wait for the receipt to be durable before publishing the artifact, so a crash
                // never leaves a consumed-but-uncached receipt.
                batch.write_batch_receipt(receipt.clone()).wait().await;
                receipt
            }
        };

        // Decode the settled root before the receipt moves into the artifact slot.
        let settled_root = BatchTransition::ref_from_bytes(&B::journal_bytes(&receipt))
            .expect("batch receipt journal decodes")
            .new_state;

        // Publish the receipt as the batch's artifact.
        batch.publish_artifact(Some(receipt));

        // Wait for this batch's commit so the next batch sees the committed prev_state, but stay
        // shutdown-escapable so a wedged latch during teardown cannot hang the join. On shutdown
        // the commit may never land, so bail before the compare below: the checkpoint would
        // still hold the pre-batch root, which the compare would misread as a divergence.
        tokio::select! {
            biased;
            () = self.prover.shutdown.wait() => return,
            () = batch.wait_committed() => {}
        }

        // `wait_committed` also resolves on cancellation, so the batch may have been canceled
        // while its proof was in flight or rolled back right after its commit: the store then
        // roots the checkpoint at an earlier state, which is not a divergence. A rollback can
        // also land between the commit and the root read, so re-check after the read too (a
        // rollback after it cannot change the value already read).
        if batch.canceled() {
            return;
        }
        let host_root = self.store.root(batch.checkpoint().index());
        if batch.canceled() {
            return;
        }

        // The receipt is proven and the host commit is final, so this is where the settled root
        // and the host root first become comparable. A disagreement is a guest/host state
        // divergence: continuing would let the next bundle's prev_state assert wedge the lane,
        // so fail loudly at the batch that caused it.
        assert_eq!(settled_root, host_root, "settled new_state diverges from the host store root");
    }

    /// Returns the first entry field in which the cached `receipt` differs from `batch`, or `None`
    /// when the receipt enters exactly where the batch does.
    ///
    /// The entry is what a fresh proof of the batch commits as its starting point: the state
    /// root at `prev_version` and the lane pins the block's metadata carries. A receipt whose
    /// journal does not decode enters nowhere.
    fn entry_mismatch(
        &self,
        batch: &ScheduledBatch<S, P>,
        prev_version: u64,
        receipt: &B::Receipt,
    ) -> Option<&'static str> {
        let journal = B::journal_bytes(receipt);
        let Ok(transition) = BatchTransition::ref_from_bytes(&journal) else {
            return Some("undecodable journal");
        };
        let metadata = batch.checkpoint().metadata();
        if transition.prev_state != self.store.root(prev_version) {
            Some("state root")
        } else if transition.prev_lane_tip != metadata.prev_lane_tip {
            Some("lane tip")
        } else if transition.prev_lane_blue_score.get() != metadata.prev_lane_blue_score {
            Some("lane blue score")
        } else if (transition.lane_expired != 0) != metadata.lane_expired {
            Some("lane expiry")
        } else {
            None
        }
    }
}
