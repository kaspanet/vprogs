use alloc::vec::Vec;

use borsh::{BorshDeserialize, BorshSerialize};

use crate::Access;

/// Generic L2 transaction wrapper that carries an opaque inner payload `T` alongside
/// pre-parsed resource accesses for scheduling.
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub struct L2Transaction<T> {
    pub l1_tx: T,
    pub resources: Vec<Access>,
}
