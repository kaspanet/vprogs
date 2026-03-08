#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod account;
pub mod batch_witness;
mod block_metadata;
mod error;
pub mod guest;
#[cfg(feature = "host")]
pub mod host;
mod storage_op;

pub use account::Account;
pub use block_metadata::BlockMetadata;
pub use error::{Error, Result};
pub use storage_op::StorageOp;

/// Fixed header size: tx_index(4) + n_accounts(4) + block_hash(32) + blue_score(8) +
/// tx_bytes_len(4).
pub const FIXED_HEADER_SIZE: usize = 4 + 4 + 32 + 8 + 4;

/// Per-account header size: resource_id(32) + flags(1) + account_index(4) + data_len(4).
pub const ACCOUNT_HEADER_SIZE: usize = 32 + 1 + 4 + 4;
