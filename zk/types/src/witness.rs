use alloc::vec::Vec;

use borsh::{BorshDeserialize, BorshSerialize};

use crate::AccountInput;

/// Owned witness data for passing to ZK backends.
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub struct TransactionContextWitness {
    pub tx_bytes: Vec<u8>,
    pub tx_index: u32,
    pub batch_metadata: Vec<u8>,
    pub accounts: Vec<AccountInput>,
}
