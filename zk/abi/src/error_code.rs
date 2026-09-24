use crate::Error;

/// Guest error codes carried by [`Error::Guest`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum ErrorCode {
    /// Transaction protocol version is not supported by this prover build.
    VersionIncompatible = 1,
    /// Unknown exit destination tag in a journal exit entry.
    InvalidExitSpkTag = 2,
    /// `ScriptPublicKey` is not one of the supported variants (Schnorr P2PK / ECDSA P2PK / P2SH).
    InvalidExitSpk = 3,
    /// Guest aborted without producing a journal; synthesized host-side when the executor call
    /// fails.
    GuestPanic = 4,
    /// The transaction wrote a resource it declared `Read`.
    ReadDeclaredWrite = 5,
}

impl From<ErrorCode> for Error {
    fn from(e: ErrorCode) -> Self {
        Error::Guest(e as u32)
    }
}
