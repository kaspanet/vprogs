use risc0_zkvm::{ExecutorEnv, ProverOpts, Receipt, default_executor, default_prover};
use vprogs_zk_proof_provider::{BackendError, ZkBackend};

/// Shared executor helper — builds env and runs the guest.
fn execute_guest(
    elf: &[u8],
    witness_bytes: &[u8],
) -> Result<risc0_zkvm::SessionInfo, BackendError> {
    let env = ExecutorEnv::builder()
        .write(&witness_bytes.to_vec())
        .map_err(|e| BackendError::Failed(e.to_string()))?
        .build()
        .map_err(|e| BackendError::Failed(e.to_string()))?;

    default_executor().execute(env, elf).map_err(|e| BackendError::Failed(e.to_string()))
}

/// RISC-0 backend for execution and proving.
///
/// In dev mode (`RISC0_DEV_MODE=1`), `prove()` generates fake receipts suitable for testing.
/// Without dev mode, it produces real succinct proofs.
#[derive(Clone)]
pub struct Risc0Prover;

impl ZkBackend for Risc0Prover {
    type Receipt = Receipt;

    fn execute(&self, elf: &[u8], witness_bytes: &[u8]) -> Result<Vec<u8>, BackendError> {
        execute_guest(elf, witness_bytes).map(|session| session.journal.bytes)
    }

    fn prove(&self, elf: &[u8], witness_bytes: &[u8]) -> Result<Receipt, BackendError> {
        let env = ExecutorEnv::builder()
            .write(&witness_bytes.to_vec())
            .map_err(|e| BackendError::Failed(e.to_string()))?
            .build()
            .map_err(|e| BackendError::Failed(e.to_string()))?;

        let prove_info = default_prover()
            .prove_with_opts(env, elf, &ProverOpts::succinct())
            .map_err(|e| BackendError::Failed(e.to_string()))?;

        Ok(prove_info.receipt)
    }

    fn journal_bytes(receipt: &Receipt) -> Vec<u8> {
        receipt.journal.bytes.clone()
    }
}
