use risc0_zkvm::{
  ExecutorEnv, FakeReceipt, InnerReceipt, ProverOpts, Receipt, default_executor, default_prover,
};
use vprogs_zk_proof_provider::{BackendError, ZkBackend};

include!(concat!(env!("OUT_DIR"), "/methods.rs"));

/// Re-exported guest ELF and image ID.
pub const GUEST_ELF: &[u8] = VPROGS_ZK_RISC0_GUEST_ELF;
pub const GUEST_ID: [u32; 8] = VPROGS_ZK_RISC0_GUEST_ID;

/// Re-exported stitcher ELF and image ID.
pub const STITCHER_ELF: &[u8] = VPROGS_ZK_RISC0_STITCHER_ELF;
pub const STITCHER_ID: [u32; 8] = VPROGS_ZK_RISC0_STITCHER_ID;

/// Shared executor helper — builds env and runs the guest.
fn execute_guest(
  elf: &[u8],
  witness_bytes: &[u8],
) -> Result<risc0_zkvm::SessionInfo, BackendError> {
  let env = ExecutorEnv::builder()
    .write_slice(witness_bytes)
    .build()
    .map_err(|e| BackendError::Failed(e.to_string()))?;

  default_executor().execute(env, elf).map_err(|e| BackendError::Failed(e.to_string()))
}

/// RISC-0 backend that produces real succinct proofs.
#[derive(Clone)]
pub struct Risc0Prover;

impl ZkBackend for Risc0Prover {
  type Receipt = Receipt;

  fn execute(&self, elf: &[u8], witness_bytes: &[u8]) -> Result<Vec<u8>, BackendError> {
    execute_guest(elf, witness_bytes).map(|session| session.journal.bytes)
  }

  fn prove(&self, elf: &[u8], witness_bytes: &[u8]) -> Result<Receipt, BackendError> {
    let env = ExecutorEnv::builder()
      .write_slice(witness_bytes)
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

/// RISC-0 backend that runs execution only (no real proof) — suitable for testing.
#[derive(Clone)]
pub struct Risc0MockBackend;

impl ZkBackend for Risc0MockBackend {
  type Receipt = Receipt;

  fn execute(&self, elf: &[u8], witness_bytes: &[u8]) -> Result<Vec<u8>, BackendError> {
    execute_guest(elf, witness_bytes).map(|session| session.journal.bytes)
  }

  fn prove(&self, elf: &[u8], witness_bytes: &[u8]) -> Result<Receipt, BackendError> {
    let session = execute_guest(elf, witness_bytes)?;

    Ok(Receipt::new(
      InnerReceipt::Fake(FakeReceipt::new(session.receipt_claim.unwrap())),
      session.journal.bytes,
    ))
  }

  fn journal_bytes(receipt: &Receipt) -> Vec<u8> {
    receipt.journal.bytes.clone()
  }
}
