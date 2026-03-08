use alloc::vec::Vec;

use super::StorageOp;
use crate::Result;

/// Deserializes the borsh-encoded execution result from guest stdout.
///
/// The wire format is a borsh `Result<Vec<Option<StorageOp>>, Error>`: discriminant `0` for
/// success followed by the storage ops, or `1` for failure followed by the error code.
///
/// # Panics
///
/// Panics if deserialization fails, which indicates a malicious or corrupted guest ELF.
pub fn decode(bytes: &[u8]) -> Result<Vec<Option<StorageOp>>> {
    borsh::from_slice(bytes).expect("failed to decode guest output")
}
