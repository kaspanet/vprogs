use std::sync::Arc;

use risc0_zkvm::{ExecutorEnv, ProverOpts, Receipt, default_executor, default_prover};

/// RISC-0 backend for execution and proving.
///
/// Owns the transaction and batch ELF binaries. In dev mode (`RISC0_DEV_MODE=1`),
/// `prove_transaction()` generates fake receipts suitable for testing.
#[derive(Clone)]
pub struct Backend {
    /// ELF binary for single-transaction execution and proving.
    transaction_elf: Arc<Vec<u8>>,
    /// ELF binary for batch aggregation proving.
    batch_elf: Arc<Vec<u8>>,
}

impl Backend {
    /// Creates a new backend from the given guest ELF binaries.
    pub fn new(transaction_elf: Vec<u8>, batch_elf: Vec<u8>) -> Self {
        Self { transaction_elf: Arc::new(transaction_elf), batch_elf: Arc::new(batch_elf) }
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
