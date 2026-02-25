mod types;

use tokio::sync::mpsc;
pub use types::*;
use vprogs_scheduling_scheduler::{RuntimeBatch, TransactionContext, VmInterface};
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;
use vprogs_zk_types::{AccountInput, ProofRequest, StateOp, Witness};

/// Errors returned by ZK backend operations.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("{0}")]
    Failed(String),
}

/// Abstraction over a zero-knowledge VM backend (e.g. RISC-0, SP1).
///
/// Implementations own their ELF binaries and receive a typed [`Witness`].
/// The associated [`Receipt`](ZkBackend::Receipt) type is backend-specific.
pub trait ZkBackend: Clone + Send + Sync + 'static {
    /// The proof receipt type produced by this backend.
    type Receipt: Send + Sync + 'static;

    /// Execute a transaction from a witness snapshot. Returns one optional state op per account.
    fn execute(&self, witness: &Witness) -> Result<Vec<Option<StateOp>>, BackendError>;

    /// Prove a previously executed transaction from its witness.
    fn prove_transaction(&self, witness: &Witness) -> Result<Self::Receipt, BackendError>;

    /// Prove a batch of transactions.
    fn prove_batch(
        &self,
        batch_index: u64,
        journals: &[Vec<u8>],
    ) -> Result<Self::Receipt, BackendError>;

    /// Extract journal bytes from a receipt.
    fn journal_bytes(receipt: &Self::Receipt) -> Vec<u8>;
}

/// Extension trait for snapshotting a [`TransactionContext`] into an owned [`Witness`].
pub trait IntoWitness {
    fn into_witness(&self) -> Witness;
}

impl<S: Store<StateSpace = StateSpace>, B: ZkBackend> IntoWitness
    for TransactionContext<'_, S, ZkVm<B>>
{
    fn into_witness(&self) -> Witness {
        Witness {
            tx_bytes: self.transaction().tx_bytes.clone(),
            tx_index: self.tx_index(),
            batch_metadata: borsh::to_vec(self.batch_metadata()).unwrap(),
            accounts: self
                .resources()
                .iter()
                .map(|r| AccountInput {
                    account_id: borsh::to_vec(&r.access_metadata().id).unwrap(),
                    data: r.data().clone(),
                    version: r.version(),
                })
                .collect(),
        }
    }
}

/// ZK VM that executes programs via a [`ZkBackend`] and optionally sends proof requests
/// to a proving pipeline.
#[derive(Clone)]
pub struct ZkVm<B: ZkBackend> {
    backend: B,
    proof_tx: Option<mpsc::UnboundedSender<ProofRequest>>,
}

impl<B: ZkBackend> ZkVm<B> {
    /// Creates a new ZK VM with the given backend.
    pub fn new(backend: B) -> Self {
        Self { backend, proof_tx: None }
    }

    /// Creates a new ZK VM that sends proof requests to the given channel after execution.
    pub fn with_proof_channel(backend: B, proof_tx: mpsc::UnboundedSender<ProofRequest>) -> Self {
        Self { backend, proof_tx: Some(proof_tx) }
    }
}

impl<B: ZkBackend> VmInterface for ZkVm<B> {
    fn process_transaction<S: Store<StateSpace = StateSpace>>(
        &self,
        ctx: &mut TransactionContext<S, Self>,
    ) -> Result<ZkTransactionEffects, ZkVmError> {
        // 1. Snapshot context into an owned witness.
        let witness = ctx.into_witness();

        // 2. Execute via backend — returns one optional op per account.
        let ops =
            self.backend.execute(&witness).map_err(|e| ZkVmError::ExecutorFailed(e.to_string()))?;

        // 3. Apply ops to resource handles.
        for (i, op) in ops.iter().enumerate() {
            if let Some(op) = op {
                apply_op(ctx.resources_mut()[i].data_mut(), op);
            }
        }

        // 4. Collect non-None ops for effects and proof request.
        let flat_ops: Vec<StateOp> = ops.into_iter().flatten().collect();

        // 5. Optionally send a proof request to the proving pipeline.
        if let Some(ref proof_tx) = self.proof_tx {
            let _ = proof_tx.send(ProofRequest {
                batch_index: ctx.batch_metadata().batch_index,
                tx_index: ctx.tx_index(),
                witness,
                ops: flat_ops.clone(),
            });
        }

        Ok(ZkTransactionEffects { ops: flat_ops })
    }

    fn post_process_batch<S: Store<StateSpace = StateSpace>>(
        &self,
        _batch: &RuntimeBatch<S, Self>,
    ) {
    }

    type Transaction = ZkTransaction;
    type TransactionEffects = ZkTransactionEffects;
    type ResourceId = ZkResourceId;
    type AccessMetadata = ZkAccessMetadata;
    type BatchMetadata = ZkBatchMetadata;
    type Error = ZkVmError;
}

fn apply_op(data: &mut Vec<u8>, op: &StateOp) {
    match op {
        StateOp::Create { data: new_data, .. } | StateOp::Update { data: new_data, .. } => {
            data.clear();
            data.extend_from_slice(new_data);
        }
        StateOp::Delete { .. } => data.clear(),
    }
}
