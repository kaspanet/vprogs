use vprogs_zk_types::StateOp;

/// A request to prove a single transaction's execution.
///
/// Sent from the ZK VM to the proving pipeline after executor-mode verification succeeds.
#[derive(Clone, Debug)]
pub struct ProofRequest {
    pub batch_index: u64,
    pub tx_index: u32,
    pub witness_bytes: Vec<u8>,
    pub ops: Vec<Option<StateOp>>,
}
