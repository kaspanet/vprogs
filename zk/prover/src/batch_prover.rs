use std::{collections::HashMap, sync::Arc};

use tokio::sync::mpsc;
use vprogs_zk_types::ProofRequest;
use vprogs_zk_vm::Backend;

use crate::BatchProof;

/// Receives proof requests from the ZK VM, groups them by batch, and proves each transaction.
///
/// When all transactions for a batch have been proven, emits a [`BatchProof`] containing the
/// ordered journal bytes. Aggregation via the batch processor is deferred to a future milestone.
pub struct BatchProver<B: Backend> {
    backend: Arc<B>,
    rx: mpsc::UnboundedReceiver<ProofRequest>,
    batch_tx_counts: HashMap<u64, u32>,
}

impl<B: Backend + 'static> BatchProver<B> {
    pub fn new(backend: B, rx: mpsc::UnboundedReceiver<ProofRequest>) -> Self {
        Self { backend: Arc::new(backend), rx, batch_tx_counts: HashMap::new() }
    }

    /// Sets the expected transaction count for a batch so the prover knows when all proofs have
    /// been collected.
    pub fn register_batch(&mut self, batch_index: u64, tx_count: u32) {
        self.batch_tx_counts.insert(batch_index, tx_count);
    }

    /// Runs the proving loop. Receives proof requests, proves each, and emits batch proofs when
    /// all transactions for a batch have been proven.
    pub async fn run(mut self) -> mpsc::UnboundedReceiver<BatchProof> {
        let (batch_proof_tx, batch_proof_rx) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            let mut pending: HashMap<u64, Vec<(u32, B::Receipt)>> = HashMap::new();

            while let Some(request) = self.rx.recv().await {
                let backend = self.backend.clone();
                let batch_index = request.batch_index;
                let tx_index = request.tx_index;
                let witness_bytes = request.witness_bytes.clone();

                // Prove on a blocking thread.
                let receipt = match tokio::task::spawn_blocking(move || {
                    backend.prove_transaction(&witness_bytes)
                })
                .await
                {
                    Ok(Ok(receipt)) => receipt,
                    Ok(Err(e)) => {
                        log::error!("proof failed for batch {batch_index} tx {tx_index}: {e}");
                        continue;
                    }
                    Err(e) => {
                        log::error!("proof task panicked: {e}");
                        continue;
                    }
                };

                let entry = pending.entry(batch_index).or_default();
                entry.push((tx_index, receipt));

                // Check if all transactions for this batch have been proven.
                let expected = self.batch_tx_counts.get(&batch_index).copied();
                if expected.is_some_and(|n| entry.len() as u32 >= n) {
                    let mut receipts = pending.remove(&batch_index).unwrap();
                    receipts.sort_by_key(|(idx, _)| *idx);

                    let inner_receipts =
                        receipts.into_iter().map(|(_, r)| B::journal_bytes(&r)).collect();

                    let _ = batch_proof_tx.send(BatchProof { batch_index, inner_receipts });
                    self.batch_tx_counts.remove(&batch_index);
                }
            }
        });

        batch_proof_rx
    }
}
