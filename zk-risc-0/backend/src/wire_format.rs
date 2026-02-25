use risc0_zkvm::ExecutorEnv;
use vprogs_zk_vm::BackendError;

/// Builds a RISC-0 executor environment with the rkyv-serialized witness bytes.
///
/// Uses `write` with serde framing to match `env::read::<Vec<u8>>()` on the guest side.
pub fn build_env(witness_bytes: &[u8]) -> Result<ExecutorEnv<'static>, BackendError> {
    ExecutorEnv::builder()
        .write(&witness_bytes.to_vec())
        .map_err(|e| BackendError::Failed(e.to_string()))?
        .build()
        .map_err(|e| BackendError::Failed(e.to_string()))
}
