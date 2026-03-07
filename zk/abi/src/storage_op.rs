use alloc::vec::Vec;

use borsh::{BorshDeserialize, BorshSerialize};

/// A storage mutation produced by executing a transaction, addressed by account index.
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub enum StorageOp {
    Create(Vec<u8>),
    Update(Vec<u8>),
    Delete,
}

/// A borrowing variant of [`StorageOp`] that serializes with the **same** borsh wire format.
///
/// This lets the guest stream account data directly from backing/promoted buffers without
/// cloning. The host deserializes as `Vec<Option<StorageOp>>` unchanged.
pub enum StorageOpRef<'a> {
    Create(&'a [u8]),
    Update(&'a [u8]),
    Delete,
}

impl BorshSerialize for StorageOpRef<'_> {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> borsh::io::Result<()> {
        match self {
            StorageOpRef::Create(data) | StorageOpRef::Update(data) => {
                let variant = if matches!(self, StorageOpRef::Create(_)) { 0u8 } else { 1u8 };
                // Batch variant byte + length prefix into a single write.
                let mut header = [0u8; 5];
                header[0] = variant;
                header[1..5].copy_from_slice(&(data.len() as u32).to_le_bytes());
                writer.write_all(&header)?;
                writer.write_all(data)?;
            }
            StorageOpRef::Delete => {
                writer.write_all(&[2])?;
            }
        }
        Ok(())
    }
}
