use vprogs_zk_abi::{Result, StorageOp};

/// A request to prove a single transaction's execution.
///
/// Sent from the ZK VM to the proving pipeline after executor-mode execution completes.
/// Stores the pre-encoded wire bytes so the prover can feed them directly to the backend.
#[derive(Clone, Debug)]
pub struct ProofRequest {
    /// Pre-encoded ABI wire bytes for the transaction.
    pub wire_bytes: Vec<u8>,
    /// Hash of the block this transaction belongs to.
    pub block_hash: [u8; 32],
    /// Index of the transaction within the batch.
    pub tx_index: u32,
    /// Execution result: storage operations on success, guest error on failure.
    pub execution_result: Result<Vec<Option<StorageOp>>>,
}
