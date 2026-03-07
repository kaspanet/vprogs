use std::sync::Arc;

use risc0_zkvm::{ExecutorEnv, ProverOpts, Receipt, default_executor, default_prover};
use vprogs_zk_abi::{StorageOp, host};
use vprogs_zk_vm::{Error, Result};

/// RISC-0 backend for execution and proving.
///
/// Owns the transaction and batch ELF binaries. In dev mode (`RISC0_DEV_MODE=1`),
/// `prove_transaction()` generates fake receipts suitable for testing.
#[derive(Clone)]
pub struct Backend {
    transaction_elf: Arc<Vec<u8>>,
    batch_elf: Arc<Vec<u8>>,
}

impl Backend {
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
            .map_err(|e| Error::Backend(e.to_string()))?;

        default_executor()
            .execute(env, &self.transaction_elf)
            .map_err(|e| Error::Backend(e.to_string()))?;

        Ok(host::decode_execution_result(&ops_stdout)?)
    }

    fn prove_transaction(&self, wire_bytes: &[u8]) -> Result<Receipt> {
        let env = ExecutorEnv::builder()
            .write_slice(&[wire_bytes.len() as u32])
            .write_slice(wire_bytes)
            .build()
            .map_err(|e| Error::Backend(e.to_string()))?;

        default_prover()
            .prove_with_opts(env, &self.transaction_elf, &ProverOpts::succinct())
            .map(|info| info.receipt)
            .map_err(|e| Error::Backend(e.to_string()))
    }

    fn prove_batch(&self, _block_hash: [u8; 32], _journals: &[Vec<u8>]) -> Result<Receipt> {
        let _elf = &self.batch_elf;
        Err(Error::Backend("batch proving not yet implemented".into()))
    }

    fn journal_bytes(receipt: &Receipt) -> Vec<u8> {
        receipt.journal.bytes.clone()
    }
}
