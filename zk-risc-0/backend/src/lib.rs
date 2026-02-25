use std::sync::Arc;

use risc0_zkvm::{ExecutorEnv, ProverOpts, Receipt, default_executor, default_prover};
use vprogs_zk_types::{StateOp, Witness};
use vprogs_zk_vm::{BackendError, ZkBackend};

/// RISC-0 backend for execution and proving.
///
/// Owns the transaction and batch ELF binaries. In dev mode (`RISC0_DEV_MODE=1`),
/// `prove_transaction()` generates fake receipts suitable for testing.
#[derive(Clone)]
pub struct Risc0Backend {
    transaction_elf: Arc<Vec<u8>>,
    batch_elf: Arc<Vec<u8>>,
}

impl Risc0Backend {
    pub fn new(transaction_elf: Vec<u8>, batch_elf: Vec<u8>) -> Self {
        Self { transaction_elf: Arc::new(transaction_elf), batch_elf: Arc::new(batch_elf) }
    }
}

impl ZkBackend for Risc0Backend {
    type Receipt = Receipt;

    fn execute(&self, witness: &Witness) -> Result<Vec<Option<StateOp>>, BackendError> {
        let wire_bytes = serialize_witness(witness);
        let env = build_env(&wire_bytes)?;
        let _session = default_executor()
            .execute(env, &self.transaction_elf)
            .map_err(|e| BackendError::Failed(e.to_string()))?;
        // Guest currently produces empty ops.
        Ok(vec![None; witness.accounts.len()])
    }

    fn prove_transaction(&self, witness: &Witness) -> Result<Receipt, BackendError> {
        let wire_bytes = serialize_witness(witness);
        let env = build_env(&wire_bytes)?;
        default_prover()
            .prove_with_opts(env, &self.transaction_elf, &ProverOpts::succinct())
            .map(|info| info.receipt)
            .map_err(|e| BackendError::Failed(e.to_string()))
    }

    fn prove_batch(
        &self,
        _batch_index: u64,
        _journals: &[Vec<u8>],
    ) -> Result<Receipt, BackendError> {
        let _elf = &self.batch_elf;
        Err(BackendError::Failed("batch proving not yet implemented".into()))
    }

    fn journal_bytes(receipt: &Receipt) -> Vec<u8> {
        receipt.journal.bytes.clone()
    }
}

/// Serializes a [`Witness`] into the flat wire format consumed by the guest program.
///
/// Wire format (all integers little-endian, fields padded to 4-byte alignment):
/// ```text
/// [total_len: u32]
/// [tx_bytes_len: u32] [tx_bytes: u8*] [pad to align(4)]
/// [tx_index: u32]
/// [batch_metadata_len: u32] [batch_metadata: u8*] [pad to align(4)]
/// [num_accounts: u32]
///   for each account:
///     [account_id_len: u32] [account_id: u8*] [pad to align(4)]
///     [data_len: u32] [data: u8*] [pad to align(4)]
///     [version: u64]
/// ```
fn serialize_witness(witness: &Witness) -> Vec<u8> {
    let mut buf = Vec::new();
    // Reserve space for total_len (will be filled at the end).
    buf.extend_from_slice(&0u32.to_le_bytes());

    write_bytes_aligned(&mut buf, &witness.tx_bytes);
    buf.extend_from_slice(&witness.tx_index.to_le_bytes());
    write_bytes_aligned(&mut buf, &witness.batch_metadata);

    buf.extend_from_slice(&(witness.accounts.len() as u32).to_le_bytes());
    for account in &witness.accounts {
        write_bytes_aligned(&mut buf, &account.account_id);
        write_bytes_aligned(&mut buf, &account.data);
        buf.extend_from_slice(&account.version.to_le_bytes());
    }

    // Fill in total_len.
    let total_len = buf.len() as u32;
    buf[..4].copy_from_slice(&total_len.to_le_bytes());

    buf
}

fn write_bytes_aligned(buf: &mut Vec<u8>, data: &[u8]) {
    buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
    buf.extend_from_slice(data);
    let rem = buf.len() % 4;
    if rem != 0 {
        buf.resize(buf.len() + (4 - rem), 0);
    }
}

fn build_env(witness_bytes: &[u8]) -> Result<ExecutorEnv<'static>, BackendError> {
    ExecutorEnv::builder()
        .write(&witness_bytes.to_vec())
        .map_err(|e| BackendError::Failed(e.to_string()))?
        .build()
        .map_err(|e| BackendError::Failed(e.to_string()))
}
