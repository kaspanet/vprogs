use std::{future::Future, pin::Pin};

use crate::Backend;

/// Alias for a boxed, `Send`-safe, `'static` future.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Abstraction over async ZK proof generation.
///
/// Implementations may prove locally, dispatch to a thread pool, or send requests to a remote
/// proving network. Each method returns a future that resolves when the proof is ready,
/// enabling concurrent proving within the orchestrator.
pub trait ProofProvider: Clone + Send + Sync + 'static {
    /// The proof receipt type produced by this provider.
    type Receipt: Clone + Send + Sync + 'static;

    /// Proves a single transaction from pre-encoded ABI wire bytes.
    fn prove_transaction(
        &self,
        input_bytes: Vec<u8>,
    ) -> impl Future<Output = Self::Receipt> + Send + 'static;

    /// Proves an entire batch with inner transaction receipts for composition.
    fn prove_batch(
        &self,
        batch_witness: Vec<u8>,
        receipts: Vec<Self::Receipt>,
    ) -> impl Future<Output = Self::Receipt> + Send + 'static;

    /// Extracts journal bytes from a receipt.
    fn journal_bytes(receipt: &Self::Receipt) -> Vec<u8>;
}

/// Blanket implementation: any [`Backend`] is also a [`ProofProvider`] by wrapping its
/// synchronous methods in blocking tasks.
impl<B: Backend> ProofProvider for B
where
    B::Receipt: Clone,
{
    type Receipt = B::Receipt;

    fn prove_transaction(
        &self,
        input_bytes: Vec<u8>,
    ) -> impl Future<Output = Self::Receipt> + Send + 'static {
        let backend = self.clone();
        async move {
            tokio::task::spawn_blocking(move || Backend::prove_transaction(&backend, &input_bytes))
                .await
                .expect("prove_transaction task panicked")
        }
    }

    fn prove_batch(
        &self,
        batch_witness: Vec<u8>,
        receipts: Vec<Self::Receipt>,
    ) -> impl Future<Output = Self::Receipt> + Send + 'static {
        let backend = self.clone();
        async move {
            tokio::task::spawn_blocking(move || {
                let refs: Vec<&Self::Receipt> = receipts.iter().collect();
                Backend::prove_batch(&backend, &batch_witness, &refs)
            })
            .await
            .expect("prove_batch task panicked")
        }
    }

    fn journal_bytes(receipt: &Self::Receipt) -> Vec<u8> {
        B::journal_bytes(receipt)
    }
}
