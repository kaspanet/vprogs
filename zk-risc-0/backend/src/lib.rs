use risc0_binfmt::ProgramBinary;
use risc0_zkvm::{ExecutorEnv, ProverOpts, Receipt, default_executor, default_prover};
use vprogs_zk_proof_provider::{BackendError, ZkBackend};

include!(concat!(env!("OUT_DIR"), "/methods.rs"));

/// Re-exported guest ELF and image ID.
pub const GUEST_ELF: &[u8] = VPROGS_ZK_RISC0_GUEST_ELF;
pub const GUEST_ID: [u32; 8] = VPROGS_ZK_RISC0_GUEST_ID;

/// Re-exported stitcher ELF and image ID.
pub const STITCHER_ELF: &[u8] = VPROGS_ZK_RISC0_STITCHER_ELF;
pub const STITCHER_ID: [u32; 8] = VPROGS_ZK_RISC0_STITCHER_ID;

/// Wraps a raw RISC-V ELF (as produced by `cargo build --target riscv32im-risc0-zkvm-elf`)
/// into the `ProgramBinary` format expected by the RISC-0 executor.
///
/// The normal `risc0-build` pipeline does this automatically, but Docker-built ELFs
/// need this post-processing step.
pub fn wrap_raw_elf(user_elf: &[u8]) -> Vec<u8> {
    let kernel_elf = risc0_zkos_v1compat::V1COMPAT_ELF;
    ProgramBinary::new(user_elf, kernel_elf).encode()
}

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
