use rkyv::util::AlignedVec;
use vprogs_zk_abi::StateOp;
use vprogs_zk_vm::Result;

/// Reads the length-prefixed rkyv-serialized ops from the guest's stdout buffer.
///
/// Copies into an [`AlignedVec`] first — the stdout buffer is an unaligned `&[u8]`,
/// but rkyv zero-copy access requires alignment matching the archived types.
pub(crate) fn read_ops(stdout_buf: &[u8]) -> Result<Vec<Option<StateOp>>> {
    let len = u32::from_le_bytes(stdout_buf[..4].try_into().unwrap()) as usize;
    let mut aligned: AlignedVec = AlignedVec::with_capacity(len);
    aligned.extend_from_slice(&stdout_buf[4..4 + len]);
    Ok(rkyv::from_bytes::<Vec<Option<StateOp>>, rkyv::rancor::Error>(&aligned)?)
}
