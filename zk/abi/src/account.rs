use alloc::vec::Vec;

use rkyv::{Archive, Serialize};

/// A snapshot of a single account's state at execution time.
#[derive(Clone, Debug, Archive, Serialize)]
pub struct Account {
    pub account_id: Vec<u8>,
    pub data: Vec<u8>,
}
