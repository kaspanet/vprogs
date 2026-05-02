use alloc::string::String;
use core::fmt::Display;

/// Wire format decode error.
#[derive(Clone, Debug, thiserror::Error)]
pub enum Error {
    /// A field could not be decoded. Contains the field name for diagnostics.
    #[error("decode error: {0}")]
    Decode(&'static str),
    /// A zerocopy reinterpretation of the underlying bytes failed.
    #[error("zerocopy: {0}")]
    Zerocopy(String),
}

impl<A: Display, S: Display, V: Display> From<zerocopy::ConvertError<A, S, V>> for Error {
    fn from(e: zerocopy::ConvertError<A, S, V>) -> Self {
        Self::Zerocopy(alloc::format!("{e}"))
    }
}

/// Result type with [`Error`] as the default error.
pub type Result<T> = core::result::Result<T, Error>;
