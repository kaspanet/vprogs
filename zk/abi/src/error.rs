use borsh::{BorshDeserialize, BorshSerialize};

/// An error produced by guest program execution.
///
/// Uses a numeric code rather than a string to minimize proven cycles in the zkVM.
#[derive(Clone, Copy, Debug, BorshSerialize, BorshDeserialize, thiserror::Error)]
#[error("execution error: code {0}")]
pub struct Error(pub u32);

/// Result type for ZK transaction execution.
pub type Result<T> = core::result::Result<T, Error>;
