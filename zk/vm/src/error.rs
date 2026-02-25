/// Errors returned by the ZK VM during transaction processing.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("witness serialization failed: {0}")]
    WitnessSerialization(String),
    #[error("executor failed: {0}")]
    ExecutorFailed(String),
    #[error("journal extraction failed")]
    JournalExtraction,
}
