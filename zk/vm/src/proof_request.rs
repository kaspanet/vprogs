use vprogs_zk_abi::StateOp;

/// A request to prove a single transaction's execution.
///
/// Sent from the ZK VM to the proving pipeline after executor-mode verification succeeds.
#[derive(Clone, Debug)]
pub struct ProofRequest {
    pub block_hash: [u8; 32],
    pub tx_index: u32,
    pub witness_bytes: Vec<u8>,
    pub ops: Vec<Option<StateOp>>,
}
