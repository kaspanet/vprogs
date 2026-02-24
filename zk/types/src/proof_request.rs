use alloc::vec::Vec;

use borsh::{BorshDeserialize, BorshSerialize};

/// A request to prove a single transaction's execution.
///
/// Sent from [`ZkVm`](crate) to the proving pipeline after executor-mode verification succeeds.
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub struct ProofRequest {
    pub batch_index: u64,
    pub tx_index: u32,
    pub journal_bytes: Vec<u8>,
    pub witness_bytes: Vec<u8>,
}
