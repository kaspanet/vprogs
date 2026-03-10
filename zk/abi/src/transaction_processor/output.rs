use alloc::vec::Vec;

use super::{Resource, StorageOp};
use crate::{Result, Write};

/// Decoded execution result from the transaction processor guest.
pub struct Output(Vec<Option<StorageOp>>);

impl Output {
    /// Host-side: decode execution result from guest.
    #[cfg(feature = "host")]
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes[0] == 1 {
            // Ok variant.
            let len =
                u32::from_le_bytes(bytes[1..5].try_into().expect("truncated output")) as usize;
            let mut ops = Vec::with_capacity(len);
            let mut offset = 5;
            for _ in 0..len {
                let (op, new_offset) = StorageOp::decode_option(bytes, offset);
                ops.push(op);
                offset = new_offset;
            }
            Ok(Self(ops))
        } else {
            // Err variant.
            let error_code = u32::from_le_bytes(bytes[1..5].try_into().expect("truncated output"));
            Err(crate::Error(error_code))
        }
    }

    /// Guest-side: encode execution result to host stream.
    pub(crate) fn encode(result: Result<&[Resource<'_>]>, w: &mut impl Write) {
        match result {
            Ok(resources) => {
                w.write(&[1]); // Ok discriminant
                w.write(&(resources.len() as u32).to_le_bytes());
                for resource in resources {
                    StorageOp::encode_option(w, resource);
                }
            }
            Err(err) => {
                w.write(&[0]); // Err discriminant
                w.write(&err.0.to_le_bytes());
            }
        }
    }

    /// Returns the storage operations.
    pub fn ops(&self) -> &[Option<StorageOp>] {
        &self.0
    }
}
