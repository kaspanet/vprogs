use alloc::vec::Vec;

use crate::{
    Result, Write,
    transaction_processor::{Resource, StorageOp},
};

/// Decoded execution result from the transaction processor guest.
pub struct Outputs {
    storage_ops: Vec<Option<StorageOp>>,
}

impl Outputs {
    /// Returns the storage operations.
    pub fn storage_ops(&self) -> &[Option<StorageOp>] {
        &self.storage_ops
    }

    /// Guest-side: encode execution result to host stream.
    pub(crate) fn encode(result: Result<&[Resource<'_>]>, w: &mut impl Write) {
        match result {
            Ok(resources) => {
                // Write Ok discriminant
                w.write(&[0]);

                // Write length-prefixed list of storage operations.
                w.write(&(resources.len() as u32).to_le_bytes());
                for resource in resources {
                    StorageOp::encode(w, resource);
                }
            }
            Err(err) => {
                // Write Err discriminant.
                w.write(&[1]);

                // Write error code as u32.
                w.write(&err.0.to_le_bytes());
            }
        }
    }

    /// Host-side: decode execution result from guest.
    #[cfg(feature = "host")]
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        match bytes[0] {
            0 => {
                // Decode length-prefixed list of storage operations.
                let count = u32::from_le_bytes(bytes[1..5].try_into().expect("count truncated"));
                let mut buf = &bytes[5..];
                let mut storage_ops = Vec::with_capacity(count as usize);
                for _ in 0..storage_ops.capacity() {
                    storage_ops.push(StorageOp::decode(&mut buf));
                }
                Ok(Self { storage_ops })
            }
            _ => Err(crate::Error(u32::from_le_bytes(
                bytes[1..5].try_into().expect("error code truncated"),
            ))),
        }
    }
}
