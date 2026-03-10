use std::{collections::HashMap, sync::Arc};

use tokio::sync::mpsc;
use vprogs_zk_abi::{
    batch_processor::input::{Header, encode},
    transaction_processor::{Input, Output, Resource, ResourceInputCommitment, StorageOp},
};
use vprogs_zk_smt::EMPTY_LEAF_HASH;
use vprogs_zk_vm::{Backend, ProofRequest};

use crate::{
    BatchProof, batch_state::BatchState, resource_data::ResourceData, state_tree::StateTree,
};

/// Receives proof requests from the ZK VM, proves individual transactions, and assembles
/// batch witnesses for the batch processor guest.
///
/// When all transactions for a batch have been proven, constructs the batch witness from
/// pre-batch resource data, generates a multi-proof from the state tree, and calls the
/// backend's `prove_batch` to produce the batch proof.
pub struct BatchProver<B: Backend> {
    backend: Arc<B>,
    rx: mpsc::UnboundedReceiver<ProofRequest>,
    batch_tx_counts: HashMap<[u8; 32], u32>,
    /// The transaction processor's image ID, committed to in the batch witness.
    image_id: [u8; 32],
    /// Host-side state tree for Merkle root maintenance and multi-proof generation.
    state_tree: StateTree,
    /// Batch index counter.
    next_batch_index: u64,
}

impl<B: Backend + 'static> BatchProver<B> {
    pub fn new(backend: B, rx: mpsc::UnboundedReceiver<ProofRequest>, image_id: [u8; 32]) -> Self {
        Self {
            backend: Arc::new(backend),
            rx,
            batch_tx_counts: HashMap::new(),
            image_id,
            state_tree: StateTree::new(),
            next_batch_index: 0,
        }
    }

    /// Sets the expected transaction count for a batch so the prover knows when all proofs have
    /// been collected.
    pub fn register_batch(&mut self, block_hash: [u8; 32], tx_count: u32) {
        self.batch_tx_counts.insert(block_hash, tx_count);
    }

    /// Returns a mutable reference to the state tree for external initialization or updates.
    pub fn state_tree_mut(&mut self) -> &mut StateTree {
        &mut self.state_tree
    }

    /// Runs the proving loop. Receives proof requests, proves each transaction, and assembles
    /// batch proofs when all transactions for a batch have been proven.
    pub async fn run(mut self) -> mpsc::UnboundedReceiver<BatchProof> {
        let (batch_proof_tx, batch_proof_rx) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            let mut pending: HashMap<[u8; 32], BatchState<B::Receipt>> = HashMap::new();

            while let Some(request) = self.rx.recv().await {
                let backend = self.backend.clone();
                let block_hash = request.block_hash;
                let tx_index = request.tx_index;
                let wire_bytes_for_prove = request.wire_bytes.clone();

                // Extract pre-batch resource data from wire_bytes before proving.
                let pre_batch = extract_pre_batch_resources(&request);

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

                let batch_state = pending.entry(block_hash).or_insert_with(|| BatchState {
                    expected_tx_count: self.batch_tx_counts.get(&block_hash).copied().unwrap_or(0),
                    receipts: Vec::new(),
                    pre_batch_resources: HashMap::new(),
                });

                // Record pre-batch data for first-seen resources only.
                for (resource_index, resource_data) in pre_batch {
                    batch_state.pre_batch_resources.entry(resource_index).or_insert(resource_data);
                }

                batch_state.receipts.push((tx_index, receipt, request));

                // Check if all transactions for this batch have been proven.
                let expected = batch_state.expected_tx_count;
                if expected > 0 && batch_state.receipts.len() as u32 >= expected {
                    let batch_state = pending.remove(&block_hash).unwrap();
                    let batch_proof =
                        self.assemble_batch_proof(block_hash, batch_state, &batch_proof_tx);
                    if let Some(proof) = batch_proof {
                        let _ = batch_proof_tx.send(proof);
                    }
                    self.batch_tx_counts.remove(&block_hash);
                }
            }
        });

        batch_proof_rx
    }

    /// Assembles a batch witness, proves it, and returns a BatchProof.
    fn assemble_batch_proof(
        &mut self,
        block_hash: [u8; 32],
        mut batch_state: BatchState<B::Receipt>,
        _tx: &mpsc::UnboundedSender<BatchProof>,
    ) -> Option<BatchProof> {
        // Sort receipts by tx_index.
        batch_state.receipts.sort_by_key(|(idx, _, _)| *idx);

        let batch_index = self.next_batch_index;
        self.next_batch_index += 1;

        let prev_root = self.state_tree.root();

        // Build dense resource array ordered by resource_index.
        let n_resources = batch_state.pre_batch_resources.len();
        let mut ordered_resources: Vec<Option<&ResourceData>> = vec![None; n_resources];
        for (idx, data) in &batch_state.pre_batch_resources {
            if (*idx as usize) < n_resources {
                ordered_resources[*idx as usize] = Some(data);
            }
        }

        // Build resource commitments for the batch witness.
        // Store owned data so ResourceInputCommitment can borrow from it.
        let commitment_data: Vec<([u8; 32], [u8; 32])> = ordered_resources
            .iter()
            .map(|slot| {
                let resource = slot.expect("gap in resource_index sequence");
                let leaf_hash = if resource.data.is_empty() {
                    EMPTY_LEAF_HASH
                } else {
                    *blake3::hash(&resource.data).as_bytes()
                };
                (resource.resource_id, leaf_hash)
            })
            .collect();

        let commitments: Vec<ResourceInputCommitment<'_>> = commitment_data
            .iter()
            .enumerate()
            .map(|(i, (id, hash))| ResourceInputCommitment {
                resource_index: i as u32,
                resource_id: id,
                hash,
            })
            .collect();

        let resource_ids: Vec<[u8; 32]> = commitment_data.iter().map(|(id, _)| *id).collect();

        // Generate multi-proof from the state tree.
        let multi_proof_bytes = self.state_tree.multi_proof(&resource_ids);

        // Build transaction entries (journals only).
        let journals: Vec<Vec<u8>> =
            batch_state.receipts.iter().map(|(_, receipt, _)| B::journal_bytes(receipt)).collect();

        // Encode the batch witness.
        let header = Header {
            image_id: &self.image_id,
            batch_index,
            prev_root: &prev_root,
            n_resources: commitments.len() as u32,
            n_txs: journals.len() as u32,
        };
        let input = encode(&header, &commitments, &multi_proof_bytes, &journals);

        // Collect inner receipts for composition.
        let assumptions: Vec<&B::Receipt> =
            batch_state.receipts.iter().map(|(_, receipt, _)| receipt).collect();

        // Prove the batch.
        let receipt = self.backend.prove_batch(&input, &assumptions);

        // Apply mutations to the state tree based on execution results.
        self.apply_batch_mutations(&batch_state.receipts);

        Some(BatchProof {
            block_hash,
            batch_index,
            prev_root,
            new_root: self.state_tree.root(),
            receipt_journal: B::journal_bytes(&receipt),
        })
    }

    /// Applies state mutations from a batch's execution results to the state tree.
    fn apply_batch_mutations(&mut self, receipts: &[(u32, B::Receipt, ProofRequest)]) {
        for (_, _, request) in receipts {
            let wire = &request.wire_bytes;
            if wire.len() < Input::FIXED_HEADER_SIZE {
                continue;
            }

            let n_resources = u32::from_le_bytes(wire[4..8].try_into().unwrap()) as usize;
            let tx_bytes_len =
                u32::from_le_bytes(wire[48..Input::FIXED_HEADER_SIZE].try_into().unwrap()) as usize;
            let resources_header_start = Input::FIXED_HEADER_SIZE + tx_bytes_len;

            // Decode execution result to get mutations.
            let output = match Output::decode(&request.execution_result_bytes) {
                Ok(output) => output,
                Err(_) => continue,
            };
            let ops = output.ops();

            // Apply mutations directly to the state tree (receipts are sorted by tx_index,
            // so later mutations correctly override earlier ones for the same key).
            for (j, op) in ops.iter().enumerate().take(n_resources) {
                let base = resources_header_start + j * Resource::HEADER_SIZE;
                if base + 32 > wire.len() {
                    break;
                }
                let resource_id: [u8; 32] = wire[base..base + 32].try_into().unwrap();

                if let Some(ref op) = op {
                    match op {
                        StorageOp::Create(data) | StorageOp::Update(data) => {
                            self.state_tree.inner_mut().insert(resource_id, data);
                        }
                        StorageOp::Delete => {
                            self.state_tree.inner_mut().delete(resource_id);
                        }
                    }
                }
            }
        }
    }
}

/// Extracts pre-batch resource data from a proof request's wire_bytes.
///
/// Returns `(resource_index, ResourceData)` pairs for each resource in the transaction.
fn extract_pre_batch_resources(request: &ProofRequest) -> Vec<(u32, ResourceData)> {
    let wire = &request.wire_bytes;
    if wire.len() < Input::FIXED_HEADER_SIZE {
        return Vec::new();
    }

    let n_resources = u32::from_le_bytes(wire[4..8].try_into().unwrap()) as usize;
    let tx_bytes_len =
        u32::from_le_bytes(wire[48..Input::FIXED_HEADER_SIZE].try_into().unwrap()) as usize;
    let resources_header_start = Input::FIXED_HEADER_SIZE + tx_bytes_len;
    let payload_start = resources_header_start + n_resources * Resource::HEADER_SIZE;

    let mut result = Vec::with_capacity(n_resources);
    let mut payload_offset = payload_start;

    for (j, &resource_index) in request.resource_indices.iter().enumerate().take(n_resources) {
        let base = resources_header_start + j * Resource::HEADER_SIZE;
        if base + Resource::HEADER_SIZE > wire.len() {
            break;
        }

        let resource_id: [u8; 32] = wire[base..base + 32].try_into().unwrap();
        let data_len = u32::from_le_bytes(wire[base + 37..base + 41].try_into().unwrap()) as usize;

        let data = if payload_offset + data_len <= wire.len() {
            wire[payload_offset..payload_offset + data_len].to_vec()
        } else {
            Vec::new()
        };
        payload_offset += data_len;

        result.push((resource_index, ResourceData { resource_id, data }));
    }

    result
}
