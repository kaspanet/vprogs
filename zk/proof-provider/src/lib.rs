/// Errors returned by ZK backend operations.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("{0}")]
    Failed(String),
}

/// Abstraction over a zero-knowledge VM backend (e.g. RISC-0, SP1).
///
/// Implementations provide execution-only mode (for scheduling) and proving mode (for proof
/// generation). The associated [`Receipt`](ZkBackend::Receipt) type is backend-specific.
pub trait ZkBackend: Clone + Send + Sync + 'static {
    /// The proof receipt type produced by this backend.
    type Receipt: Send + Sync + 'static;

    /// Execute a guest program without generating a proof. Returns journal bytes.
    fn execute(&self, elf: &[u8], witness_bytes: &[u8]) -> Result<Vec<u8>, BackendError>;

    /// Prove a guest program execution. Returns a receipt containing the proof and journal.
    fn prove(&self, elf: &[u8], witness_bytes: &[u8]) -> Result<Self::Receipt, BackendError>;

    /// Extract journal bytes from a receipt.
    fn journal_bytes(receipt: &Self::Receipt) -> Vec<u8>;
}
