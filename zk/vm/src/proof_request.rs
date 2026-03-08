/// A request to prove a single transaction's execution.
///
/// Sent from the ZK VM to the proving pipeline after executor-mode execution completes.
/// Stores the pre-encoded wire bytes and raw execution result so the prover can feed
/// them directly to the backend.
#[derive(Clone, Debug)]
pub struct ProofRequest {
    /// Pre-encoded ABI wire bytes for the transaction.
    pub wire_bytes: Vec<u8>,
    /// Hash of the block this transaction belongs to.
    pub block_hash: [u8; 32],
    /// Index of the transaction within the batch.
    pub tx_index: u32,
    /// Raw execution result bytes from the guest.
    pub execution_result_bytes: Vec<u8>,
    /// Per-resource indices within the batch (one per resource accessed by this tx).
    pub resource_indices: Vec<u32>,
}
