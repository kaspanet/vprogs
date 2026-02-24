use alloc::vec::Vec;

use borsh::{BorshDeserialize, BorshSerialize};

/// A snapshot of a single account's state at execution time.
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub struct AccountInput {
    pub account_id: Vec<u8>,
    pub data: Vec<u8>,
    pub version: u64,
}
