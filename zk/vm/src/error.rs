/// Result type for the ZK VM.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors returned by the ZK VM during transaction processing.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("backend failed: {0}")]
    Backend(String),
    #[error("rkyv failed: {0}")]
    Rkyv(String),
}

impl From<rkyv::rancor::Error> for Error {
    fn from(e: rkyv::rancor::Error) -> Self {
        Self::Rkyv(e.to_string())
    }
}
