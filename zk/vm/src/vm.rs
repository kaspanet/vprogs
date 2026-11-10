use std::sync::Arc;

use tokio::sync::mpsc;
use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};
use vprogs_scheduling_scheduler::{Processor, ScheduledBatch, TransactionContext};
use vprogs_storage_types::Store;
use vprogs_zk_abi::{
    Error, Result,
    transaction_processor::{Inputs, JournalEntries, OutputCommitment, Outputs},
    withdrawal::ExitLeaf,
};

use crate::{Backend, ProvingPipeline};

/// One tx's committed exit leaves tagged with their journal position: the executing batch's
/// sequential id and the tx's zero-based position within that batch. The pair totally orders
/// leaves into journal order regardless of arrival order across worker threads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExitsForBatch {
    /// Sequential batch id of the executing batch.
    pub batch_id: u64,
    /// Zero-based position of the tx within the batch.
    pub tx_position: u32,
    /// Leaves the tx's journal committed, in journal order.
    pub leaves: Vec<ExitLeaf>,
}

/// ZK processor that executes programs and optionally coordinates proving via [`ProvingPipeline`].
#[derive(Clone)]
pub struct Vm<B: Backend, S: Store> {
    /// The ZK backend used for execution and proving.
    backend: B,
    /// Proving strategy (None, Transaction-only, or full Batch).
    proving_pipeline: Arc<ProvingPipeline<S, Self>>,
    /// Optional sink for per-tx exit leaves extracted from tx journals, tagged with the
    /// executing batch's id and the tx's position in it. Wired only by the exec node: the
    /// proving node's aggregate prover already publishes per-bundle exits, and a second
    /// emission there would double-feed the exit indexer.
    exits_tap: Option<mpsc::UnboundedSender<ExitsForBatch>>,
}

impl<B: Backend, S: Store> Vm<B, S> {
    /// Creates a new ZK VM with the given backend and proving pipeline, without an exits tap.
    pub fn new(backend: B, proving_pipeline: ProvingPipeline<S, Self>) -> Self {
        Self { backend, proving_pipeline: Arc::new(proving_pipeline), exits_tap: None }
    }

    /// Wires the per-tx exits tap, consuming and returning `self`.
    pub fn with_exits_tap(mut self, tap: mpsc::UnboundedSender<ExitsForBatch>) -> Self {
        self.exits_tap = Some(tap);
        self
    }

    /// Sends the tx journal's committed exits into the tap under `(batch_id, tx_position)`,
    /// when one is wired. A journal that fails to decode drops that tx's exits with a warning
    /// instead of failing the tx: stdout already validated the execution, so a malformed exit
    /// blob is a codebase mismatch the downstream root attribution would surface anyway.
    // ponytail: no in-flight dedup; a leaf message already queued when the reorg marker lands
    // keeps a stale copy that duplicates the replacement's re-emission (root attribution
    // blocks, never mis-attributes) until a later marker prunes it; dedup by tx id if a
    // reorg-heavy lane ever needs it.
    fn tap_exits(&self, batch_id: u64, tx_position: u32, journal_bytes: &[u8]) {
        let Some(tap) = &self.exits_tap else { return };
        let decoded = JournalEntries::decode(journal_bytes).and_then(|entries| {
            match entries.output_commitment {
                OutputCommitment::Success { exits, .. } => {
                    exits.iter().collect::<Result<Vec<_>>>().map(|pairs| {
                        pairs
                            .into_iter()
                            .map(|(dest, amount)| ExitLeaf::from_pair(dest, amount))
                            .collect::<Vec<_>>()
                    })
                }
                // A failed tx commits no exits; its error surfaces via `Outputs::decode` below.
                OutputCommitment::Error(_) => Ok(Vec::new()),
            }
        });
        match decoded {
            Ok(leaves) if !leaves.is_empty() => {
                // Receiver dropped means no consumer is listening; silently ignore.
                let _ = tap.send(ExitsForBatch { batch_id, tx_position, leaves });
            }
            Ok(_) => {}
            Err(e) => log::warn!("vm exits tap: undecodable tx journal exits: {e:?}"),
        }
    }
}

impl<B: Backend, S: Store> Processor<S> for Vm<B, S> {
    fn process_transaction(&self, ctx: &mut TransactionContext<S, Self>) -> Result<()> {
        // Encode into ABI wire format.
        let input_bytes = Inputs::encode(&*ctx);

        // Execute via backend. The journal carries the output commitment (including any emitted
        // exits) alongside the stdout stream.
        let outcome = self.backend.execute_transaction(&input_bytes);

        // Forward the journal's exits before applying storage ops; the decode borrows the journal
        // buffer and converts to owned leaves immediately.
        self.tap_exits(ctx.batch().checkpoint().index(), ctx.batch_position(), &outcome.journal);

        // Submit to proving pipeline (no-op if ProvingPipeline::None).
        self.proving_pipeline.submit_transaction(ctx.scheduled_tx(), input_bytes);

        // Decode and apply storage operations.
        Outputs::decode(&outcome.stdout, ctx.resources().len()).map(|output| {
            for (resource, op) in ctx.resources_mut().iter_mut().zip(output.storage_ops) {
                if let Some(new_data) = op {
                    resource.set_data(new_data);
                    log::trace!(
                        "executed: resource_index={} version={} data={}",
                        resource.resource_index(),
                        resource.version(),
                        faster_hex::hex_string(resource.data()),
                    );
                }
            }
        })
    }

    fn on_batch_scheduled(&self, batch: &ScheduledBatch<S, Self>) {
        self.proving_pipeline.submit_batch(batch);
    }

    fn on_rollback(&self, target_index: u64) {
        self.proving_pipeline.rollback(target_index);
    }

    fn on_shutdown(&self) {
        self.proving_pipeline.shutdown();
    }

    fn tx_image_id(&self) -> [u8; 32] {
        *self.backend.image_id()
    }

    fn batch_image_id(&self) -> [u8; 32] {
        *self.backend.batch_image_id()
    }

    /// Restore is safe only without proving and without an exits tap: any proving mode needs
    /// per-tx pre-images, and a restored block skips execution, so leaves a rollback pruned
    /// never re-emit.
    fn supports_restore(&self) -> bool {
        self.exits_tap.is_none() && matches!(*self.proving_pipeline, ProvingPipeline::None)
    }

    type Transaction = L1Transaction;
    type TransactionArtifact = B::Receipt;
    type BatchArtifact = B::Receipt;
    type AggregatorArtifact = B::Receipt;
    type BatchMetadata = ChainBlockMetadata;
    type Error = Error;
}
