mod types;
pub mod witness_builder;

use tokio::sync::mpsc;
pub use types::*;
use vprogs_scheduling_scheduler::{AccessHandle, RuntimeBatch, TransactionContext, VmInterface};
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;
use vprogs_zk_types::ProofRequest;

/// Errors returned by ZK backend operations.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("{0}")]
    Failed(String),
}

/// Result of executing a transaction through a ZK backend.
pub struct ExecutionResult {
    pub journal_bytes: Vec<u8>,
    pub witness_bytes: Vec<u8>,
}

/// Abstraction over a zero-knowledge VM backend (e.g. RISC-0, SP1).
///
/// Implementations own their ELF binaries and receive scheduler types directly.
/// The associated [`Receipt`](ZkBackend::Receipt) type is backend-specific.
pub trait ZkBackend: Clone + Send + Sync + 'static {
    /// The proof receipt type produced by this backend.
    type Receipt: Send + Sync + 'static;

    /// Execute a transaction. Backend receives scheduler types directly,
    /// builds witness internally, returns journal + captured witness bytes.
    fn execute_transaction<S: Store<StateSpace = StateSpace>>(
        &self,
        resources: &[AccessHandle<S, ZkVm<Self>>],
        ctx: &TransactionContext<ZkVm<Self>>,
    ) -> Result<ExecutionResult, BackendError>;

    /// Prove a previously executed transaction from captured witness bytes.
    fn prove_transaction(&self, witness_bytes: &[u8]) -> Result<Self::Receipt, BackendError>;

    /// Prove a batch of transactions.
    fn prove_batch(
        &self,
        batch_index: u64,
        journals: &[Vec<u8>],
    ) -> Result<Self::Receipt, BackendError>;

    /// Extract journal bytes from a receipt.
    fn journal_bytes(receipt: &Self::Receipt) -> Vec<u8>;
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
        resources: &mut [AccessHandle<S, Self>],
        ctx: &TransactionContext<Self>,
    ) -> Result<ZkTransactionEffects, ZkVmError> {
        // 1. Execute via backend (builds witness internally, returns journal + witness).
        let result = self
            .backend
            .execute_transaction(resources, ctx)
            .map_err(|e| ZkVmError::ExecutorFailed(e.to_string()))?;

        // 2. Optionally send a proof request to the proving pipeline.
        if let Some(ref proof_tx) = self.proof_tx {
            let _ = proof_tx.send(ProofRequest {
                batch_index: ctx.batch_metadata().batch_index,
                tx_index: ctx.tx_index(),
                journal_bytes: result.journal_bytes.clone(),
                witness_bytes: result.witness_bytes,
            });
        }

        Ok(ZkTransactionEffects { journal_bytes: result.journal_bytes })
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
