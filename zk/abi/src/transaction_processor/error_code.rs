use crate::Error;

/// Transaction processor verification error codes emitted as `OutputCommitment::Error` payloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum ErrorCode {
    /// Transaction's protocol version is not supported by this prover build. Emitted by the
    /// guest after committing only the minimal `(tx_id, version, merge_idx)` input header.
    VersionIncompatible = 1,
    /// Host-supplied `tx_id` does not match the cryptographically derived value for this tx's
    /// version. Emitted only on the executable path; signals host bug or tampering.
    TxIdMismatch = 2,
    MissingExecutionInputs = 3,
}

impl From<ErrorCode> for Error {
    fn from(e: ErrorCode) -> Self {
        Error::Guest(e as u32)
    }
}
