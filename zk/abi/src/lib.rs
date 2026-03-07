#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod account;
mod block_metadata;
pub mod guest;
#[cfg(feature = "host")]
pub mod host;
#[cfg(feature = "host")]
mod host_error;
mod storage_op;

pub use account::Account;
pub use block_metadata::BlockMetadata;
#[cfg(feature = "host")]
pub use host_error::{HostError, HostResult};
pub use storage_op::{StorageOp, StorageOpRef};

pub(crate) const FIXED_HEADER_SIZE: usize = 4 + 4 + 32 + 8 + 4;
pub(crate) const ACCOUNT_HEADER_SIZE: usize = 32 + 1 + 4;
