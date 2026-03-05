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
            StorageOpRef::Create(data) => {
                // Variant index 0
                writer.write_all(&[0])?;
                // Length-prefixed bytes (same as Vec<u8> borsh format)
                writer.write_all(&(data.len() as u32).to_le_bytes())?;
                writer.write_all(data)?;
            }
            StorageOpRef::Update(data) => {
                // Variant index 1
                writer.write_all(&[1])?;
                writer.write_all(&(data.len() as u32).to_le_bytes())?;
                writer.write_all(data)?;
            }
            StorageOpRef::Delete => {
                // Variant index 2
                writer.write_all(&[2])?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_op_ref_borsh_compat() {
        let data = vec![1u8, 2, 3, 4, 5];

        // Create
        let owned = StorageOp::Create(data.clone());
        let borrowed = StorageOpRef::Create(&data);
        assert_eq!(borsh::to_vec(&owned).unwrap(), borsh::to_vec(&borrowed).unwrap());

        // Update
        let owned = StorageOp::Update(data.clone());
        let borrowed = StorageOpRef::Update(&data);
        assert_eq!(borsh::to_vec(&owned).unwrap(), borsh::to_vec(&borrowed).unwrap());

        // Delete
        let owned = StorageOp::Delete;
        let borrowed = StorageOpRef::Delete;
        assert_eq!(borsh::to_vec(&owned).unwrap(), borsh::to_vec(&borrowed).unwrap());

        // Option<StorageOp> compat
        let owned_some: Option<StorageOp> = Some(StorageOp::Update(data.clone()));
        let borrowed_some: Option<StorageOpRef> = Some(StorageOpRef::Update(&data));
        assert_eq!(borsh::to_vec(&owned_some).unwrap(), borsh::to_vec(&borrowed_some).unwrap());

        let owned_none: Option<StorageOp> = None;
        let borrowed_none: Option<StorageOpRef> = None;
        assert_eq!(borsh::to_vec(&owned_none).unwrap(), borsh::to_vec(&borrowed_none).unwrap());
    }
}
