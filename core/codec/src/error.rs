use core::fmt;

/// Errors from wire format parsing and proof traversal.
#[derive(Clone, Debug)]
pub enum Error {
    /// A wire format field could not be decoded. Wraps the field name.
    Decode(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(field) => write!(f, "decode error: {field}"),
        }
    }
}

/// Result type with [`Error`] as the default error.
pub type Result<T> = core::result::Result<T, Error>;
