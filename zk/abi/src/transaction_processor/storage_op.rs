use alloc::vec::Vec;

/// A storage mutation produced by executing a transaction, addressed by resource index.
#[derive(Clone, Debug)]
pub enum StorageOp {
    Create(Vec<u8>),
    Update(Vec<u8>),
    Delete,
}

impl StorageOp {
    pub(crate) const CREATE: u8 = 0;
    pub(crate) const UPDATE: u8 = 1;
    pub(crate) const DELETE: u8 = 2;

    /// Encodes a resource as `Option<StorageOp>`, translating dirty/deleted/new flags into the
    /// corresponding variant. Batches the variant byte and length prefix into a single 5-byte
    /// write to minimize I/O calls.
    pub(crate) fn encode_option(w: &mut impl crate::Write, resource: &super::Resource<'_>) {
        if resource.is_deleted() {
            w.write(&[1, Self::DELETE]);
        } else if resource.is_dirty() {
            let data = resource.data();
            let variant = if resource.is_new() { Self::CREATE } else { Self::UPDATE };
            let mut header = [1u8, 0, 0, 0, 0, 0];
            header[1] = variant;
            header[2..6].copy_from_slice(&(data.len() as u32).to_le_bytes());
            w.write(&header);
            w.write(data);
        } else {
            w.write(&[0]);
        }
    }

    /// Decodes an `Option<StorageOp>` from `buf` at `offset`, returning the value and new offset.
    #[cfg(feature = "host")]
    pub(crate) fn decode_option(buf: &[u8], offset: usize) -> (Option<Self>, usize) {
        if buf[offset] == 0 {
            return (None, offset + 1);
        }
        let variant = buf[offset + 1];
        match variant {
            Self::CREATE | Self::UPDATE => {
                let len = u32::from_le_bytes(
                    buf[offset + 2..offset + 6].try_into().expect("truncated storage op"),
                ) as usize;
                let data = buf[offset + 6..offset + 6 + len].to_vec();
                let op =
                    if variant == Self::CREATE { Self::Create(data) } else { Self::Update(data) };
                (Some(op), offset + 6 + len)
            }
            Self::DELETE => (Some(Self::Delete), offset + 2),
            _ => panic!("unknown StorageOp variant: {variant}"),
        }
    }
}
