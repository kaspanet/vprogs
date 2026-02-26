use vprogs_zk_abi::{StateOp, TransactionContext};

/// A request to prove a single transaction's execution.
///
/// Sent from the ZK VM to the proving pipeline after executor-mode verification succeeds.
#[derive(Clone, Debug)]
pub struct ProofRequest {
    pub abi_ctx: TransactionContext,
    pub ops: Vec<Option<StateOp>>,
}
