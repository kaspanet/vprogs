use crate::Error;

/// Transaction processor error codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum ErrorCode {
    /// Transaction protocol version is not supported by this prover build.
    VersionIncompatible = 1,
    /// Host-supplied `tx_id` does not match the cryptographically derived value.
    TxIdMismatch = 2,
    /// Version is supported but `execution_input` was not supplied.
    MissingExecutionInputs = 3,
}

impl From<ErrorCode> for Error {
    fn from(e: ErrorCode) -> Self {
        Error::Guest(e as u32)
    }
}
