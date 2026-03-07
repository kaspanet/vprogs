/// Result type for the ZK VM.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors returned by the ZK VM during transaction processing.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("backend failed: {0}")]
    Backend(String),
    #[error(transparent)]
    Abi(#[from] vprogs_zk_abi::HostError),
}
