/// Wire format decode error.
#[derive(Clone, Debug, thiserror::Error)]
pub enum Error {
    /// A field could not be decoded. Contains the field name for diagnostics.
    #[error("decode error: {0}")]
    Decode(&'static str),
}

/// Result type with [`Error`] as the default error.
pub type Result<T> = core::result::Result<T, Error>;
