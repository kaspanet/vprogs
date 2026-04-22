use crate::Error;

/// Batch processor verification error codes.
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
    /// Inner transaction journal verification failed.
    JournalVerificationFailed = 6,
    /// DAA score differs from the first transaction in the batch.
    DaaScoreMismatch = 7,
    /// Block header timestamp differs from the first transaction in the batch.
    TimestampMismatch = 8,
    /// Previous block's timestamp differs from the first transaction in the batch.
    PrevTimestampMismatch = 9,
    /// Transaction subnetwork differs from the lane this batch binds to.
    SubnetworkMismatch = 10,
}

impl From<ErrorCode> for Error {
    fn from(e: ErrorCode) -> Self {
        Error::Guest(e as u32)
    }
}
