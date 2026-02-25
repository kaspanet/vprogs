use risc0_zkvm::ExecutorEnv;
use vprogs_zk_types::TransactionContextWitness;
use vprogs_zk_vm::BackendError;

/// Serializes a [`TransactionContextWitness`] into the flat wire format consumed by the guest
/// program.
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
pub fn serialize_witness(witness: &TransactionContextWitness) -> Vec<u8> {
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

pub fn write_bytes_aligned(buf: &mut Vec<u8>, data: &[u8]) {
    buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
    buf.extend_from_slice(data);
    let rem = buf.len() % 4;
    if rem != 0 {
        buf.resize(buf.len() + (4 - rem), 0);
    }
}

pub fn build_env(witness_bytes: &[u8]) -> Result<ExecutorEnv<'static>, BackendError> {
    ExecutorEnv::builder()
        .write(&witness_bytes.to_vec())
        .map_err(|e| BackendError::Failed(e.to_string()))?
        .build()
        .map_err(|e| BackendError::Failed(e.to_string()))
}
