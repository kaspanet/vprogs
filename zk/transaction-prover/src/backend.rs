use std::future::Future;

/// ZK backend for transaction proving.
pub trait Backend: Clone + Send + Sync + 'static {
    /// Proof receipt type produced by this backend.
    type Receipt: Clone + Send + Sync + 'static;

    /// Returns the guest image ID.
    fn image_id(&self) -> &[u8; 32];

    /// Proves a transaction from pre-encoded wire bytes.
    fn prove_transaction(
        &self,
        input_bytes: Vec<u8>,
    ) -> impl Future<Output = Self::Receipt> + Send + 'static;
}
