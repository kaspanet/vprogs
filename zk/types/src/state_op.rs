use alloc::vec::Vec;

use borsh::{BorshDeserialize, BorshSerialize};
use rkyv::{Archive, Deserialize, Serialize};

/// A state mutation produced by executing a transaction, addressed by account index.
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize, Archive, Serialize, Deserialize)]
pub enum StateOp {
    Create(Vec<u8>),
    Update(Vec<u8>),
    Delete,
}
