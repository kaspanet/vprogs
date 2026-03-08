use std::sync::Arc;

use risc0_zkvm::{ExecutorEnv, ProverOpts, Receipt, default_executor, default_prover};
use vprogs_zk_abi::{Result, StorageOp, host};

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

    fn execute_transaction(&self, wire_bytes: &[u8]) -> Result<Vec<Option<StorageOp>>> {
        let mut ops_stdout = Vec::new();
        let env = ExecutorEnv::builder()
            .write_slice(&[wire_bytes.len() as u32])
            .write_slice(wire_bytes)
            .stdout(&mut ops_stdout)
            .build()
            .expect("failed to build executor environment");

        default_executor().execute(env, &self.transaction_elf).expect("executor failed");

        host::decode_execution_result(&ops_stdout)
    }

    fn prove_transaction(&self, wire_bytes: &[u8]) -> Result<Receipt> {
        let env = ExecutorEnv::builder()
            .write_slice(&[wire_bytes.len() as u32])
            .write_slice(wire_bytes)
            .build()
            .expect("failed to build prover environment");

        let receipt = default_prover()
            .prove_with_opts(env, &self.transaction_elf, &ProverOpts::succinct())
            .expect("proving failed")
            .receipt;

        Ok(receipt)
    }

    fn prove_batch(&self, _block_hash: [u8; 32], _journals: &[Vec<u8>]) -> Result<Receipt> {
        let _elf = &self.batch_elf;
        unimplemented!("batch proving not yet implemented")
    }

    fn journal_bytes(receipt: &Receipt) -> Vec<u8> {
        receipt.journal.bytes.clone()
    }
}
