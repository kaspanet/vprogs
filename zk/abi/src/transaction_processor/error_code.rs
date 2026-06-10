use crate::Error;

/// Transaction processor error codes.
///
/// The numeric discriminants share the `Error::Guest` code space with the other modules' error
/// codes (e.g. [`crate::withdrawal::ErrorCode`]); keep them globally unique.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum ErrorCode {
    /// Transaction protocol version is not supported by this prover build.
    VersionIncompatible = 1,
}

impl From<ErrorCode> for Error {
    fn from(e: ErrorCode) -> Self {
        Error::Guest(e as u32)
    }
}
