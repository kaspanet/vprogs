use alloc::vec::Vec;

use borsh::{BorshDeserialize, BorshSerialize};

use crate::{StateOp, TransactionContextWitness};

/// A request to prove a single transaction's execution.
///
/// Sent from the ZK VM to the proving pipeline after executor-mode verification succeeds.
/// `batch_index` is denormalized for routing; `tx_index` is available via the witness.
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub struct ProofRequest {
    pub batch_index: u64,
    pub witness: TransactionContextWitness,
    pub ops: Vec<Option<StateOp>>,
}
