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
    /// Wire discriminant for a successful execution.
    pub const OK: u8 = 0x00;
    /// Wire discriminant for a failed execution.
    pub const ERR: u8 = 0x01;

    /// Returns the storage operations.
    pub fn storage_ops(&self) -> &[Option<StorageOp>] {
        &self.storage_ops
    }

    /// Decodes the execution result from the guest (host-side).
    #[cfg(feature = "host")]
    pub fn decode(mut buf: &[u8]) -> Result<Self> {
        use vprogs_core_utils::Parser;

        use crate::Error;

        // Dispatch based on discriminant.
        match buf.consume_u8("discriminant")? {
            Self::OK => {
                // Decode length-prefixed storage operations.
                let count = buf.consume_u32("count")? as usize;
                let mut storage_ops = Vec::with_capacity(count);
                for _ in 0..count {
                    storage_ops.push(StorageOp::decode(&mut buf)?);
                }

                Ok(Self { storage_ops })
            }
            Self::ERR => Err(Error::decode(&mut buf)?),
            _ => Err(Error::Decode("invalid output discriminant".into())),
        }
    }

    /// Encodes the execution result to the host stream (guest-side).
    pub fn encode(result: &Result<&[Resource<'_>]>, w: &mut impl Write) {
        match *result {
            Ok(resources) => {
                // Write Ok discriminant.
                w.write(&[Self::OK]);

                // Write length-prefixed list of storage operations.
                w.write(&(resources.len() as u32).to_le_bytes());
                for resource in resources {
                    StorageOp::encode(w, resource);
                }
            }
            Err(ref err) => {
                // Write Err discriminant and encode error.
                w.write(&[Self::ERR]);
                err.encode(w);
            }
        }
    }
}
