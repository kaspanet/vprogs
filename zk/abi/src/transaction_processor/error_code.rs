use crate::Error;

/// Transaction processor error codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum ErrorCode {
    /// Transaction protocol version is not supported by this prover build.
    VersionIncompatible = 1,
    /// Unknown exit destination tag in a journal exit entry.
    InvalidExitSpkTag = 2,
    /// `ScriptPublicKey` is not one of the supported variants (Schnorr P2PK / ECDSA P2PK / P2SH).
    InvalidExitSpk = 3,
}

impl From<ErrorCode> for Error {
    fn from(e: ErrorCode) -> Self {
        Error::Guest(e as u32)
    }
}
