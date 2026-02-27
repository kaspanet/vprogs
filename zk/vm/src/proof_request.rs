use vprogs_zk_abi::{StorageOp, TransactionContext};

/// A request to prove a single transaction's execution.
///
/// Sent from the ZK VM to the proving pipeline after executor-mode verification succeeds.
#[derive(Clone, Debug)]
pub struct ProofRequest {
    pub tx_ctx: TransactionContext,
    pub storage_ops: Vec<Option<StorageOp>>,
}
