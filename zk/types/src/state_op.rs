use alloc::vec::Vec;

use borsh::{BorshDeserialize, BorshSerialize};

/// A state mutation produced by executing a transaction, addressed by account index.
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub enum StateOp {
    Create(Vec<u8>),
    Update(Vec<u8>),
    Delete,
}
