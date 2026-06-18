use alloc::vec::Vec;

use vprogs_core_codec::Writer;

use crate::{Result, transaction_processor::Effects};

/// Decoded execution result from the transaction processor guest.
pub struct Outputs {
    /// Per-resource storage mutations; `None` for unchanged resources.
    pub storage_ops: Vec<Option<Vec<u8>>>,
}

impl Outputs {
    /// Wire discriminant for a successful execution.
    pub const OK: u8 = 0x00;
    /// Wire discriminant for a failed execution.
    pub const ERR: u8 = 0x01;

    /// Returns the storage operations.
    pub fn storage_ops(&self) -> &[Option<Vec<u8>>] {
        &self.storage_ops
    }

    /// Decodes the execution result with `count` resources from the guest (host-side).
    #[cfg(feature = "host")]
    pub fn decode(mut buf: &[u8], count: usize) -> Result<Self> {
        use vprogs_core_codec::Reader;

        use crate::Error;

        // Dispatch based on discriminant.
        match buf.byte("discriminant")? {
            Self::OK => {
                // Decode one storage op per accessed resource.
                let mut storage_ops = Vec::with_capacity(count);
                for _ in 0..count {
                    storage_ops.push(match buf.bool("dirty_flag")? {
                        true => Some(buf.blob("data")?.to_vec()),
                        false => None,
                    });
                }

                Ok(Self { storage_ops })
            }
            Self::ERR => Err(Error::decode(&mut buf)?),
            _ => Err(Error::Decode("invalid output discriminant".into())),
        }
    }

    /// Encodes the execution result to the host stream (guest-side).
    pub fn encode(result: &Result<Effects<'_>>, w: &mut impl Writer) {
        match *result {
            Ok(Effects { resources, .. }) => {
                // Write Ok discriminant.
                w.write(&[Self::OK]);

                // Write one storage op per resource.
                for resource in resources {
                    match resource.is_dirty() {
                        true => {
                            w.write(&[1]);
                            w.write_blob(resource.data());
                        }
                        false => w.write(&[0]),
                    }
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
