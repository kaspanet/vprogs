use alloc::vec::Vec;

use crate::transaction_processor::Resource;

/// A storage mutation produced by executing a transaction, addressed by resource index.
#[derive(Clone, Debug)]
pub enum StorageOp {
    /// A new resource was created with the given data.
    Create(Vec<u8>),
    /// An existing resource was updated with new data.
    Update(Vec<u8>),
    /// An existing resource was deleted.
    Delete,
}

impl StorageOp {
    /// Wire variant byte for [`Create`](Self::Create).
    pub const CREATE: u8 = 0;
    /// Wire variant byte for [`Update`](Self::Update).
    pub const UPDATE: u8 = 1;
    /// Wire variant byte for [`Delete`](Self::Delete).
    pub const DELETE: u8 = 2;

    /// Decodes an `Option<StorageOp>`, advancing `buf` past the consumed bytes.
    #[cfg(feature = "host")]
    pub fn decode(buf: &mut &[u8]) -> crate::Result<Option<Self>> {
        use vprogs_core_codec::Reader;

        // Read dirty flag. Unchanged (non-dirty) resources are encoded as a single 0 byte.
        if !buf.bool("dirty_flag")? {
            return Ok(None);
        }

        // Decode variant.
        match buf.byte("variant")? {
            Self::DELETE => Ok(Some(Self::Delete)),
            variant => {
                // Decode length prefix.
                let data_length = buf.le_u32("data_length")? as usize;

                // Read the data and construct the corresponding variant.
                Ok(Some(if variant == Self::CREATE {
                    Self::Create(buf.bytes(data_length, "data")?.to_vec())
                } else {
                    Self::Update(buf.bytes(data_length, "data")?.to_vec())
                }))
            }
        }
    }

    /// Encodes a resource as `Option<StorageOp>`, translating dirty/deleted/new flags into the
    /// corresponding variant.
    pub fn encode(w: &mut impl crate::Write, resource: &Resource<'_>) {
        // Write unchanged (non-dirty) resources as a single 0 byte.
        if !resource.is_dirty() {
            w.write(&[0]);
            return;
        }

        // Write deleted resources as a dirty flag + delete variant (without length prefix or data).
        if resource.is_deleted() {
            w.write(&[1, Self::DELETE]);
            return;
        }

        // Write new/updated resources as a dirty flag + variant + length prefix + data.
        let variant = if resource.is_new() { Self::CREATE } else { Self::UPDATE };
        let data = resource.data();
        let mut header_buf = [1u8, variant, 0, 0, 0, 0];
        header_buf[2..6].copy_from_slice(&(data.len() as u32).to_le_bytes());
        w.write(&header_buf);
        w.write(data);
    }
}
