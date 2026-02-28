#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

use borsh::{BorshDeserialize, BorshSerialize};

use crate::Access;

/// Generic L2 transaction wrapper that carries an opaque inner payload `T` alongside
/// pre-parsed resource accesses for scheduling.
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub struct L2Transaction<T> {
    pub inner: T,
    pub accesses: Vec<Access>,
}

impl<T> L2Transaction<T> {
    pub fn accessed_resources(&self) -> &[Access] {
        &self.accesses
    }
}
