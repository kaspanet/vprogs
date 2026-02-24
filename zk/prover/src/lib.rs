use std::{collections::HashMap, sync::Arc};

use risc0_zkvm::Receipt;
use tokio::sync::mpsc;
use vprogs_zk_proof_provider::ProofProvider;
use vprogs_zk_types::ProofRequest;

/// Result of proving an entire batch.
pub struct BatchProof {
    pub batch_index: u64,
    /// Borsh-serialized inner receipts (one per transaction, ordered by tx_index).
    pub inner_receipts: Vec<Vec<u8>>,
}

/// Receives proof requests from the ZK VM, groups them by batch, and proves each transaction.
///
/// When all transactions for a batch have been proven, emits a [`BatchProof`] containing the
/// ordered receipts. Aggregation via the stitcher guest is deferred to a future milestone.
pub struct BatchProver<P: ProofProvider> {
    provider: Arc<P>,
    inner_elf: Arc<Vec<u8>>,
    rx: mpsc::UnboundedReceiver<ProofRequest>,
    batch_tx_counts: HashMap<u64, u32>,
}

impl<P: ProofProvider + 'static> BatchProver<P> {
    pub fn new(provider: P, inner_elf: Vec<u8>, rx: mpsc::UnboundedReceiver<ProofRequest>) -> Self {
        Self {
            provider: Arc::new(provider),
            inner_elf: Arc::new(inner_elf),
            rx,
            batch_tx_counts: HashMap::new(),
        }
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
            let mut pending: HashMap<u64, Vec<(u32, Receipt)>> = HashMap::new();

            while let Some(request) = self.rx.recv().await {
                let provider = self.provider.clone();
                let elf = self.inner_elf.clone();
                let witness_bytes = request.witness_bytes.clone();
                let batch_index = request.batch_index;
                let tx_index = request.tx_index;

                // Prove on a blocking thread.
                let receipt =
                    match tokio::task::spawn_blocking(move || provider.prove(&elf, &witness_bytes))
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

                    let inner_receipts = receipts
                        .into_iter()
                        .map(|(_, r)| borsh::to_vec(&r.journal.bytes).unwrap_or_default())
                        .collect();

                    let _ = batch_proof_tx.send(BatchProof { batch_index, inner_receipts });
                    self.batch_tx_counts.remove(&batch_index);
                }
            }
        });

        batch_proof_rx
    }
}
