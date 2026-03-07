/// Errors from host-side ABI operations.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("deserialization failed: {0}")]
    Deserialization(String),
}

pub type HostResult<T> = Result<T, HostError>;
