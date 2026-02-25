use risc0_zkvm::ExecutorEnv;
use vprogs_zk_vm::BackendError;

/// Builds a RISC-0 executor environment with the rkyv-serialized witness bytes.
///
/// Uses `write_slice` for zero-overhead transfer — a u32 length header followed by
/// raw bytes, no serde. Matched by `read_slice` calls on the guest side.
pub fn build_env(witness_bytes: &[u8]) -> Result<ExecutorEnv<'static>, BackendError> {
    ExecutorEnv::builder()
        .write_slice(&[witness_bytes.len() as u32])
        .write_slice(witness_bytes)
        .build()
        .map_err(|e| BackendError::Failed(e.to_string()))
}
