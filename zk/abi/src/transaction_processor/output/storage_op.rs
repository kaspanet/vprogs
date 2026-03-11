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
    pub fn decode(buf: &mut &[u8]) -> Option<Self> {
        // Read dirty flag. Unchanged (non-dirty) resources are encoded as a single 0 byte.
        let is_dirty = buf[0] != 0;
        if !is_dirty {
            *buf = &buf[1..];
            return None;
        }

        // Read variant and return if deleted.
        let variant = buf[1];
        if variant == Self::DELETE {
            *buf = &buf[2..];
            return Some(Self::Delete);
        }

        // Read length prefix and data for new/updated resources.
        let len = u32::from_le_bytes(buf[2..6].try_into().expect("truncated len")) as usize;
        let data = buf[6..6 + len].to_vec();
        *buf = &buf[6 + len..];

        // Return create vs update variant based on the variant byte.
        Some(if variant == Self::CREATE { Self::Create(data) } else { Self::Update(data) })
    }

    /// Encodes a resource as `Option<StorageOp>`, translating dirty/deleted/new flags into the
    /// corresponding variant. Batches the dirty flag, the variant byte and length prefix into a
    /// single 6-byte write to minimize I/O calls.
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
