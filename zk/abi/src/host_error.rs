/// Errors from host-side ABI operations.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("deserialization failed: {0}")]
    Deserialization(String),
}

/// Convenience alias for host-side ABI results.
pub type HostResult<T> = Result<T, HostError>;
