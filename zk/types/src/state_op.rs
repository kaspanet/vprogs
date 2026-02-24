use alloc::vec::Vec;

use borsh::{BorshDeserialize, BorshSerialize};

/// A state mutation produced by executing a transaction.
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub enum StateOp {
    Create { account_id: Vec<u8>, data: Vec<u8> },
    Update { account_id: Vec<u8>, data: Vec<u8> },
    Delete { account_id: Vec<u8> },
}
