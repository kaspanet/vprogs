use alloc::vec::Vec;

use crate::{
    Result, Write,
    transaction_processor::{Resource, StorageOp},
};

/// Decoded execution result from the transaction processor guest.
pub struct Outputs {
    /// Per-resource storage mutations; `None` for unchanged resources.
    pub storage_ops: Vec<Option<StorageOp>>,
}

impl Outputs {
    /// Returns the storage operations.
    pub fn storage_ops(&self) -> &[Option<StorageOp>] {
        &self.storage_ops
    }

    /// Decodes the execution result from the guest (host-side).
    #[cfg(feature = "host")]
    pub fn decode(buf: &[u8]) -> Result<Self> {
        use crate::Parser;

        match buf[0] {
            0 => {
                // Decode length-prefixed storage operations.
                let count = buf[1..5].parse_u32("count")?;
                let mut buf = &buf[5..];
                let mut storage_ops = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    storage_ops.push(StorageOp::decode(&mut buf)?);
                }

                Ok(Self { storage_ops })
            }
            _ => Err(crate::Error::Guest({
                // Decode error code.
                buf[1..5].parse_u32("error_code")?
            })),
        }
    }

    /// Encodes the execution result to the host stream (guest-side).
    pub fn encode(result: Result<&[Resource<'_>]>, w: &mut impl Write) {
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
                w.write(&err.code().to_le_bytes());
            }
        }
    }
}
