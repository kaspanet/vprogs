/// Errors from ZK ABI operations.
#[derive(Clone, Copy, Debug, thiserror::Error)]
pub enum Error {
    /// Error code returned by the guest program.
    #[error("guest error: code {0}")]
    Guest(u32),
    /// Wire format decode error.
    #[error("decode error: {0}")]
    Decode(&'static str),
}

impl Error {
    /// Returns the guest error code.
    ///
    /// Panics if this is not a [`Guest`](Self::Guest) error.
    pub fn code(&self) -> u32 {
        match self {
            Self::Guest(code) => *code,
            _ => panic!("expected guest error, got: {self}"),
        }
    }
}

/// Result type for ZK ABI operations.
pub type Result<T> = core::result::Result<T, Error>;
