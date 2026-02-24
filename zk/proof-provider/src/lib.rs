use risc0_zkvm::{ExecutorEnv, ProverOpts, Receipt, default_executor, default_prover};

/// Errors returned by proof providers.
#[derive(Debug, thiserror::Error)]
pub enum ProofError {
    #[error("proving failed: {0}")]
    ProvingFailed(String),
}

/// Trait for components that can produce a RISC-0 receipt from a guest ELF and witness.
///
/// Implementations are synchronous because proving is CPU-bound.
pub trait ProofProvider: Send + Sync + 'static {
    fn prove(&self, elf: &[u8], witness_bytes: &[u8]) -> Result<Receipt, ProofError>;
}

/// Produces real succinct proofs via the local RISC-0 prover.
pub struct LocalProofProvider;

impl ProofProvider for LocalProofProvider {
    fn prove(&self, elf: &[u8], witness_bytes: &[u8]) -> Result<Receipt, ProofError> {
        let env = ExecutorEnv::builder()
            .write_slice(witness_bytes)
            .build()
            .map_err(|e| ProofError::ProvingFailed(e.to_string()))?;

        let prove_info = default_prover()
            .prove_with_opts(env, elf, &ProverOpts::succinct())
            .map_err(|e| ProofError::ProvingFailed(e.to_string()))?;

        Ok(prove_info.receipt)
    }
}

/// Runs execution only (no real proof) — suitable for testing.
pub struct MockProofProvider;

impl ProofProvider for MockProofProvider {
    fn prove(&self, elf: &[u8], witness_bytes: &[u8]) -> Result<Receipt, ProofError> {
        let env = ExecutorEnv::builder()
            .write_slice(witness_bytes)
            .build()
            .map_err(|e| ProofError::ProvingFailed(e.to_string()))?;

        let session = default_executor()
            .execute(env, elf)
            .map_err(|e| ProofError::ProvingFailed(e.to_string()))?;

        Ok(Receipt::new(
            risc0_zkvm::InnerReceipt::Fake(risc0_zkvm::FakeReceipt::new(
                session.receipt_claim.unwrap(),
            )),
            session.journal.bytes,
        ))
    }
}
