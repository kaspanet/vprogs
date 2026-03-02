use vprogs_zk_abi::StorageOp;
use vprogs_zk_vm::{Error, Result};

/// Reads the borsh-serialized ops from the guest's stdout buffer.
pub(crate) fn read_ops(stdout_buf: &[u8]) -> Result<Vec<Option<StorageOp>>> {
    borsh::from_slice(stdout_buf).map_err(|e| Error::Deserialization(e.to_string()))
}
