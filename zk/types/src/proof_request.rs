use alloc::vec::Vec;

use borsh::{BorshDeserialize, BorshSerialize};

use crate::{StateOp, Witness};

/// A request to prove a single transaction's execution.
///
/// Sent from [`ZkVm`](crate) to the proving pipeline after executor-mode verification succeeds.
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub struct ProofRequest {
    pub batch_index: u64,
    pub tx_index: u32,
    pub witness: Witness,
    pub ops: Vec<StateOp>,
}
