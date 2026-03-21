use crate::Error;

/// Batch processor verification error codes.
///
/// Committed to the journal on failure. Each variant maps to a `u32` wire code via
/// `Error::Guest(code)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum ErrorCode {
    /// Transaction index does not match the expected sequential order.
    TxIndexMismatch = 1,
    /// Resource index is out of range for this batch.
    ResourceIndexOutOfRange = 2,
    /// Resource value hash does not match the expected pre-state.
    ResourceHashMismatch = 3,
    /// Block hash differs from the first transaction in the batch.
    BlockHashMismatch = 4,
    /// Blue score differs from the first transaction in the batch.
    BlueScoreMismatch = 5,
}

impl From<ErrorCode> for Error {
    fn from(e: ErrorCode) -> Self {
        Error::Guest(e as u32)
    }
}
