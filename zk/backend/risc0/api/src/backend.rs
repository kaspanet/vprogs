use std::sync::Arc;

use risc0_binfmt::ProgramBinary;
use risc0_zkvm::{ExecutorEnv, ProverOpts, Receipt, default_executor, default_prover};

/// RISC-0 backend for execution and proving.
///
/// Accepts raw RISC-V ELFs and wraps them with the trusted v1compat kernel.
///
/// In dev mode (`RISC0_DEV_MODE=1`), proving generates fake receipts suitable for testing.
#[derive(Clone)]
pub struct Backend {
    /// Wrapped ELF binary for single-transaction execution and proving.
    transaction_elf: Arc<Vec<u8>>,
    /// Wrapped ELF binary for batch aggregation proving.
    batch_elf: Arc<Vec<u8>>,
}

impl Backend {
    /// Creates a new backend from raw guest ELF binaries.
    ///
    /// Always wraps the provided ELFs with the trusted v1compat kernel to ensure
    /// only sanctioned syscalls are available to guest programs.
    pub fn new(transaction_elf: &[u8], batch_elf: &[u8]) -> Self {
        Self {
            transaction_elf: Arc::new(
                ProgramBinary::new(transaction_elf, risc0_zkos_v1compat::V1COMPAT_ELF).encode(),
            ),
            batch_elf: Arc::new(
                ProgramBinary::new(batch_elf, risc0_zkos_v1compat::V1COMPAT_ELF).encode(),
            ),
        }
    }
}

impl vprogs_zk_vm::Backend for Backend {
    type Receipt = Receipt;

    fn execute_transaction(&self, wire_bytes: &[u8]) -> Vec<u8> {
        let mut execution_result = Vec::new();

        default_executor()
            .execute(
                ExecutorEnv::builder()
                    .write_slice(&[wire_bytes.len() as u32])
                    .write_slice(wire_bytes)
                    .stdout(&mut execution_result)
                    .build()
                    .expect("failed to build executor environment"),
                &self.transaction_elf,
            )
            .expect("executor failed");

        execution_result
    }

    fn prove_transaction(&self, wire_bytes: &[u8]) -> Receipt {
        let env = ExecutorEnv::builder()
            .write_slice(&[wire_bytes.len() as u32])
            .write_slice(wire_bytes)
            .build()
            .expect("failed to build prover environment");

        default_prover()
            .prove_with_opts(env, &self.transaction_elf, &ProverOpts::succinct())
            .expect("proving failed")
            .receipt
    }

    fn prove_batch(&self, batch_witness: &[u8], assumptions: &[&Receipt]) -> Receipt {
        let mut builder = ExecutorEnv::builder();
        builder.write_slice(&[batch_witness.len() as u32]).write_slice(batch_witness);

        for receipt in assumptions {
            builder.add_assumption((*receipt).clone());
        }

        let env = builder.build().expect("failed to build batch prover environment");

        default_prover()
            .prove_with_opts(env, &self.batch_elf, &ProverOpts::succinct())
            .expect("batch proving failed")
            .receipt
    }

    fn journal_bytes(receipt: &Receipt) -> Vec<u8> {
        receipt.journal.bytes.clone()
    }
}
