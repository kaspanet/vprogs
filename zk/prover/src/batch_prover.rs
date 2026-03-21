use std::{collections::HashMap, sync::Arc};

use tokio::sync::mpsc;
use vprogs_storage_types::Store;
use vprogs_zk_abi::batch_processor::{Header, Inputs as BatchInputs, JournalCommitment};
use vprogs_zk_vm::{Backend, ProofRequest};

use crate::{BatchProof, batch_state::BatchState};

/// Prover error type — wraps any error from proof generation or journal decoding.
type Error = Box<dyn std::error::Error + Send + Sync>;

/// Proves individual transactions and assembles batch witnesses for the batch processor guest.
pub struct BatchProver<B: Backend, S: Store> {
    backend: Arc<B>,
    store: S,
    rx: mpsc::UnboundedReceiver<ProofRequest>,
    /// Expected tx count per batch, keyed by block_hash.
    batch_tx_counts: HashMap<[u8; 32], u32>,
    /// Store version per batch — the prover reads the pre-batch SMT state at `version - 1`.
    batch_versions: HashMap<[u8; 32], u64>,
    /// Transaction processor guest image ID.
    image_id: [u8; 32],
    /// Monotonic batch counter.
    next_batch_index: u64,
}

impl<B: Backend + 'static, S: Store> BatchProver<B, S> {
    pub fn new(
        backend: B,
        rx: mpsc::UnboundedReceiver<ProofRequest>,
        image_id: [u8; 32],
        store: S,
    ) -> Self {
        Self {
            backend: Arc::new(backend),
            store,
            rx,
            batch_tx_counts: HashMap::new(),
            batch_versions: HashMap::new(),
            image_id,
            next_batch_index: 0,
        }
    }

    /// Registers a batch so the prover knows when all proofs have been collected and which SMT
    /// version to read.
    pub fn register_batch(&mut self, block_hash: [u8; 32], tx_count: u32, version: u64) {
        self.batch_tx_counts.insert(block_hash, tx_count);
        self.batch_versions.insert(block_hash, version);
    }

    /// Runs the proving loop.
    pub async fn run(mut self) -> mpsc::UnboundedReceiver<BatchProof> {
        let (batch_proof_tx, batch_proof_rx) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            let mut pending: HashMap<[u8; 32], BatchState<B::Receipt>> = HashMap::new();

            while let Some(request) = self.rx.recv().await {
                let backend = self.backend.clone();
                let block_hash = request.block_hash;
                let tx_index = request.tx_index;
                let wire_bytes_for_prove = request.wire_bytes.clone();

                // Extract resource IDs from wire_bytes before proving.
                let pre_batch = extract_resource_ids(&request);

                // Prove on a blocking thread.
                let receipt = match tokio::task::spawn_blocking(move || {
                    backend.prove_transaction(&wire_bytes_for_prove)
                })
                .await
                {
                    Ok(receipt) => receipt,
                    Err(e) => {
                        log::error!("proof task panicked: {e}");
                        continue;
                    }
                };

                // Initialize batch state on first request for this block.
                let expected_tx_count = self.batch_tx_counts.get(&block_hash).copied().unwrap_or(0);
                let batch_state = pending.entry(block_hash).or_insert_with(|| BatchState {
                    receipts: (0..expected_tx_count).map(|_| None).collect(),
                    received: 0,
                    resource_ids: Vec::new(),
                });

                // Record resource IDs — grow the vec as needed.
                for (resource_index, resource_id) in pre_batch {
                    let idx = resource_index as usize;
                    if idx >= batch_state.resource_ids.len() {
                        batch_state.resource_ids.resize(idx + 1, [0; 32]);
                    }
                    if batch_state.resource_ids[idx] == [0; 32] {
                        batch_state.resource_ids[idx] = resource_id;
                    }
                }

                // Place receipt at its tx_index slot.
                batch_state.receipts[tx_index as usize] = Some((receipt, request));
                batch_state.received += 1;

                // All transactions proven — assemble the batch proof.
                if expected_tx_count > 0 && batch_state.received >= expected_tx_count {
                    let batch_state = pending.remove(&block_hash).unwrap();
                    match self.assemble_batch_proof(block_hash, batch_state) {
                        Ok(proof) => {
                            let _ = batch_proof_tx.send(proof);
                        }
                        Err(e) => {
                            log::error!("batch proof assembly failed: {e}");
                        }
                    }
                    self.batch_tx_counts.remove(&block_hash);
                    self.batch_versions.remove(&block_hash);
                }
            }
        });

        batch_proof_rx
    }

    /// Assembles a batch witness from collected receipts, proves it, and returns a `BatchProof`.
    fn assemble_batch_proof(
        &mut self,
        block_hash: [u8; 32],
        batch_state: BatchState<B::Receipt>,
    ) -> Result<BatchProof, Error> {
        let batch_index = self.next_batch_index;
        self.next_batch_index += 1;

        // Resolve the store version for this batch.
        let version = self.batch_versions.get(&block_hash).copied().unwrap_or(batch_index + 1);
        let prev_version = version.saturating_sub(1);

        let resource_ids = &batch_state.resource_ids;
        let n_resources = resource_ids.len();

        // Generate proof and leaf order mapping from the store at the pre-batch version.
        let (proof_bytes, leaf_order) = self.store.prove(resource_ids, prev_version)?;

        // Unwrap receipts — all slots filled since `received == expected`.
        let receipts: Vec<(B::Receipt, ProofRequest)> =
            batch_state.receipts.into_iter().map(|slot| slot.expect("missing receipt")).collect();

        // Extract journals from receipts (already in tx_index order).
        let journals: Vec<Vec<u8>> =
            receipts.iter().map(|(receipt, _)| B::journal_bytes(receipt)).collect();

        // Encode the batch witness.
        let header = Header {
            image_id: &self.image_id,
            batch_index,
            n_resources: n_resources as u32,
            n_txs: journals.len() as u32,
        };
        let input = BatchInputs::encode(&header, &leaf_order, &proof_bytes, &journals);

        // Prove the batch with inner receipts as assumptions for composition.
        let assumptions: Vec<&B::Receipt> = receipts.iter().map(|(receipt, _)| receipt).collect();
        let receipt = self.backend.prove_batch(&input, &assumptions);
        let receipt_journal = B::journal_bytes(&receipt);

        // Read roots from the guest's journal — the guest is the authority on what the proof
        // attests to.
        let (prev_root, new_root, _) = JournalCommitment::decode(&receipt_journal)?;

        Ok(BatchProof { block_hash, batch_index, prev_root, new_root, receipt_journal })
    }
}

/// Extracts `(resource_index, resource_id)` pairs from a proof request's wire_bytes.
fn extract_resource_ids(request: &ProofRequest) -> Vec<(u32, [u8; 32])> {
    use vprogs_zk_abi::transaction_processor::{Inputs, Resource};

    let wire = &request.wire_bytes;
    if wire.len() < Inputs::FIXED_HEADER_SIZE {
        return Vec::new();
    }

    let n_resources = u32::from_le_bytes(wire[4..8].try_into().unwrap()) as usize;
    let tx_bytes_len =
        u32::from_le_bytes(wire[48..Inputs::FIXED_HEADER_SIZE].try_into().unwrap()) as usize;
    let resources_header_start = Inputs::FIXED_HEADER_SIZE + tx_bytes_len;

    let mut result = Vec::with_capacity(n_resources);
    for (j, &resource_index) in request.resource_indices.iter().enumerate().take(n_resources) {
        let base = resources_header_start + j * Resource::HEADER_SIZE;
        if base + 32 > wire.len() {
            break;
        }
        let resource_id: [u8; 32] = wire[base..base + 32].try_into().unwrap();
        result.push((resource_index, resource_id));
    }

    result
}
