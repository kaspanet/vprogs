use std::{collections::HashMap, sync::Arc, thread};

use crossbeam_queue::SegQueue;
use tokio::{runtime::Builder, sync::Notify};
use vprogs_core_macros::smart_pointer;
use vprogs_scheduling_scheduler::ScheduledBatch;
use vprogs_storage_types::Store;
use vprogs_zk_abi::{
    batch_processor::{Header, Inputs as BatchInputs, StateTransition},
    transaction_processor::Inputs,
};

use crate::{Backend, BatchProof, BatchProofQueue, vm::Vm};

/// Background orchestrator that proves transactions and assembles batch proofs.
///
/// Runs two dedicated worker threads: a transaction prover that proves individual transactions
/// and a batch prover that assembles batch witnesses once all transactions in a batch are proven.
#[smart_pointer]
pub struct ProvingOrchestrator<B: Backend, S: Store> {
    /// The ZK backend used for proving.
    backend: B,
    /// Queue of transactions awaiting proving.
    task_queue: SegQueue<ProofTask<B, S>>,
    /// Wakes the transaction prover when a task is available.
    task_notify: Notify,
    /// Output queue for completed batch proofs.
    proof_queue: BatchProofQueue,
}

/// A transaction submitted for proving. The tx_index and resource metadata are encoded in
/// `input_bytes` and extracted via `Inputs::decode` on the prover thread.
struct ProofTask<B: Backend, S: Store> {
    batch: ScheduledBatch<S, Vm<B, S>>,
    input_bytes: Vec<u8>,
}

/// Per-batch proving state: accumulates receipts in the transaction prover, then moves to the
/// batch prover for assembly once complete.
struct ProvingBatch<B: Backend, S: Store> {
    /// The scheduled batch this proving state belongs to.
    batch: ScheduledBatch<S, Vm<B, S>>,
    /// Receipts indexed by tx_index, pre-allocated to `tx_count` slots.
    receipts: Vec<Option<B::Receipt>>,
    /// Number of receipts received so far.
    received: u32,
    /// Resource IDs indexed by per-batch resource_index.
    resource_ids: Vec<[u8; 32]>,
}

impl<B: Backend, S: Store> ProvingOrchestrator<B, S> {
    /// Creates a new orchestrator and starts the transaction and batch prover threads.
    pub fn new(backend: B, store: S, image_id: [u8; 32]) -> Self {
        let completed_queue = Arc::new(SegQueue::new());
        let completed_notify = Arc::new(Notify::new());

        let this = Self(Arc::new(ProvingOrchestratorData {
            backend,
            task_queue: SegQueue::new(),
            task_notify: Notify::new(),
            proof_queue: BatchProofQueue::new(),
        }));

        Self::start_transaction_prover(
            this.clone(),
            completed_queue.clone(),
            completed_notify.clone(),
        );
        Self::start_batch_prover(this.clone(), store, image_id, completed_queue, completed_notify);

        this
    }

    /// Returns the output queue for completed batch proofs.
    pub fn proof_queue(&self) -> &BatchProofQueue {
        &self.proof_queue
    }

    /// Registers a transaction for proving. Called from execution worker threads.
    pub(crate) fn register_transaction(
        &self,
        batch: &ScheduledBatch<S, Vm<B, S>>,
        input_bytes: Vec<u8>,
    ) {
        self.task_queue.push(ProofTask { batch: batch.clone(), input_bytes });
        self.task_notify.notify_one();
    }

    /// Transaction prover: pulls tasks, proves them sequentially, tracks per-batch state
    /// locally, and pushes completed batches to the batch prover.
    fn start_transaction_prover(
        orchestrator: Self,
        completed_queue: Arc<SegQueue<ProvingBatch<B, S>>>,
        completed_notify: Arc<Notify>,
    ) {
        thread::spawn(move || {
            Builder::new_current_thread().build().expect("failed to build tokio runtime").block_on(
                async move {
                    let mut pending: HashMap<[u8; 32], ProvingBatch<B, S>> = HashMap::new();

                    loop {
                        while let Some(task) = orchestrator.task_queue.pop() {
                            // Prove the transaction (blocking).
                            let receipt = orchestrator.backend.prove_transaction(&task.input_bytes);

                            let block_hash =
                                task.batch.checkpoint().metadata().block_hash().as_bytes();
                            let tx_count = task.batch.txs().len() as u32;

                            // Initialize batch state on first transaction for this block.
                            let batch = pending.entry(block_hash).or_insert_with(|| ProvingBatch {
                                batch: task.batch.clone(),
                                receipts: (0..tx_count).map(|_| None).collect(),
                                received: 0,
                                resource_ids: Vec::new(),
                            });

                            // Decode wire bytes to extract tx_index and resource IDs.
                            if let Ok(inputs) = Inputs::decode(&mut task.input_bytes.clone()) {
                                for resource in &inputs.resources {
                                    let idx = resource.index() as usize;
                                    if idx >= batch.resource_ids.len() {
                                        batch.resource_ids.resize(idx + 1, [0; 32]);
                                    }
                                    if batch.resource_ids[idx] == [0; 32] {
                                        batch.resource_ids[idx] = *resource.id().as_bytes();
                                    }
                                }

                                batch.receipts[inputs.tx_index as usize] = Some(receipt);
                            }
                            batch.received += 1;

                            // All transactions proven - push to batch prover.
                            if tx_count > 0 && batch.received >= tx_count {
                                let batch = pending.remove(&block_hash).unwrap();
                                completed_queue.push(batch);
                                completed_notify.notify_one();
                            }
                        }

                        // Exit when all external references are dropped and the queue is
                        // drained.
                        if Arc::strong_count(&orchestrator.0) == 1 {
                            break;
                        }
                        orchestrator.task_notify.notified().await;
                    }
                },
            )
        });
    }

    /// Batch prover: waits for completed batches, handles commit ordering, assembles batch
    /// witnesses, and proves them.
    fn start_batch_prover(
        orchestrator: Self,
        store: S,
        image_id: [u8; 32],
        completed_queue: Arc<SegQueue<ProvingBatch<B, S>>>,
        completed_notify: Arc<Notify>,
    ) {
        thread::spawn(move || {
            Builder::new_current_thread().build().expect("failed to build tokio runtime").block_on(
                async move {
                    let mut prev_batch: Option<ScheduledBatch<S, Vm<B, S>>> = None;

                    loop {
                        while let Some(completed) = completed_queue.pop() {
                            // Wait for the previous batch to commit before reading the store.
                            if let Some(ref prev) = prev_batch {
                                prev.wait_committed().await;
                            }

                            // Skip canceled batches.
                            if completed.batch.was_canceled() {
                                prev_batch = Some(completed.batch);
                                continue;
                            }

                            match assemble_batch_proof(
                                &orchestrator.backend,
                                &store,
                                &image_id,
                                &completed,
                            ) {
                                Ok(proof) => orchestrator.proof_queue.push(proof),
                                Err(e) => log::error!("batch proof assembly failed: {e}"),
                            }

                            prev_batch = Some(completed.batch);
                        }

                        // Exit when all external references are dropped and the queue is
                        // drained.
                        if Arc::strong_count(&completed_queue) == 1 {
                            break;
                        }
                        completed_notify.notified().await;
                    }
                },
            )
        });
    }
}

/// Assembles a batch witness from collected receipts, proves it, and returns a `BatchProof`.
fn assemble_batch_proof<B: Backend, S: Store>(
    backend: &B,
    store: &S,
    image_id: &[u8; 32],
    proving_batch: &ProvingBatch<B, S>,
) -> Result<BatchProof, Box<dyn std::error::Error + Send + Sync>> {
    let block_hash = proving_batch.batch.checkpoint().metadata().block_hash().as_bytes();
    let batch_index = proving_batch.batch.checkpoint().index();
    let prev_version = batch_index.saturating_sub(1);

    let n_resources = proving_batch.resource_ids.len();

    // Generate proof and leaf order mapping from the store at the pre-batch version.
    let (proof_bytes, leaf_order) = store.prove(&proving_batch.resource_ids, prev_version)?;

    // Extract journals from receipts (already in tx_index order).
    let receipts: Vec<&B::Receipt> =
        proving_batch.receipts.iter().map(|slot| slot.as_ref().expect("missing receipt")).collect();
    let journals: Vec<Vec<u8>> = receipts.iter().map(|r| B::journal_bytes(r)).collect();

    // Encode the batch witness.
    let header = Header {
        image_id,
        batch_index,
        n_resources: n_resources as u32,
        n_txs: receipts.len() as u32,
    };
    let input = BatchInputs::encode(&header, &leaf_order, &proof_bytes, &journals);

    // Prove the batch with inner receipts as assumptions for composition.
    let receipt = backend.prove_batch(&input, &receipts);
    let receipt_journal = B::journal_bytes(&receipt);

    // Read roots from the guest's journal.
    match StateTransition::decode(&receipt_journal)? {
        StateTransition::Success { prev_root, new_root, .. } => Ok(BatchProof {
            block_hash,
            batch_index,
            prev_root: *prev_root,
            new_root: *new_root,
            receipt_journal,
        }),
        StateTransition::Error(e) => Err(e.into()),
    }
}
