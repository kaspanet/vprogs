use vprogs_zk_abi::StorageOp;

/// A request to prove a single transaction's execution.
///
/// Sent from the ZK VM to the proving pipeline after executor-mode verification succeeds.
/// Stores the pre-encoded wire bytes so the prover can feed them directly to the backend.
#[derive(Clone, Debug)]
pub struct ProofRequest {
    pub wire_bytes: Vec<u8>,
    pub block_hash: [u8; 32],
    pub tx_index: u32,
    pub storage_ops: Vec<Option<StorageOp>>,
}
