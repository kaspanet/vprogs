use crate::Error;

/// Error codes raised while decoding exit destinations.
///
/// The numeric discriminants share the `Error::Guest` code space with the other modules' error
/// codes (e.g. [`crate::transaction_processor::ErrorCode`]); keep them globally unique.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum ErrorCode {
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
