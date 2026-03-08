use alloc::vec::Vec;

use borsh::{BorshDeserialize, BorshSerialize};

/// A storage mutation produced by executing a transaction, addressed by resource index.
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub enum StorageOp {
    Create(Vec<u8>),
    Update(Vec<u8>),
    Delete,
}

/// Borsh variant indices for [`StorageOp`], used by [`Resource`](super::Resource)'s manual
/// serializer.
impl StorageOp {
    pub(crate) const CREATE: u8 = 0;
    pub(crate) const UPDATE: u8 = 1;
    pub(crate) const DELETE: u8 = 2;
}
