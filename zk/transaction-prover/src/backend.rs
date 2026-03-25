use std::future::Future;

/// Abstraction over a zero-knowledge backend's transaction proving capabilities.
///
/// Implementations own their ELF binaries and receive pre-encoded wire bytes. Proving is async
/// to support concurrent proving, thread pools, or remote proving networks.
pub trait Backend: Clone + Send + Sync + 'static {
    /// The proof receipt type produced by this backend.
    type Receipt: Clone + Send + Sync + 'static;

    /// The future type returned by proving methods.
    type ProveFuture: Future<Output = Self::Receipt> + Send + 'static;

    /// Returns the guest image ID.
    fn image_id(&self) -> &[u8; 32];

    /// Prove a previously executed transaction from pre-encoded wire bytes.
    fn prove_transaction(&self, input_bytes: Vec<u8>) -> Self::ProveFuture;

    /// Extract journal bytes from a receipt.
    fn journal_bytes(receipt: &Self::Receipt) -> Vec<u8>;
}
