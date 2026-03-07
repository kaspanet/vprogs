use alloc::vec::Vec;

use borsh::BorshSerialize;
use vprogs_core_types::ResourceId;

use crate::storage_op::StorageOp;

/// A mutable view of a single account's data within a decoded wire buffer.
///
/// Data starts as a borrowed slice into the wire buffer (`backing`). If the caller needs more space
/// than the original slice provides, [`resize`](Account::resize) promotes to a heap-allocated `Vec`
/// (`promoted`). Reads and writes always go through the active buffer.
pub struct Account<'a> {
    /// Zero-copy reference into the wire buffer's account header.
    resource_id: &'a ResourceId,
    /// Zero-copy mutable slice into the wire buffer's payload region.
    backing: &'a mut [u8],
    /// Heap-allocated buffer, used only when the account data outgrows `backing`.
    promoted: Option<Vec<u8>>,
    /// Whether this account was created by the current transaction.
    is_new: bool,
    /// Whether the account data has been modified.
    dirty: bool,
    /// Whether the account has been marked for deletion.
    deleted: bool,
}

impl<'a> Account<'a> {
    /// Creates a new `Account` borrowing into the given slice.
    pub fn new(resource_id: &'a ResourceId, is_new: bool, backing: &'a mut [u8]) -> Self {
        Self { resource_id, is_new, backing, promoted: None, dirty: false, deleted: false }
    }

    /// Returns the account's resource identifier.
    pub fn resource_id(&self) -> &ResourceId {
        self.resource_id
    }

    /// Returns `true` if this account was created by the current transaction.
    pub fn is_new(&self) -> bool {
        self.is_new
    }

    /// Returns a shared reference to the current data (backing or promoted).
    pub fn data(&self) -> &[u8] {
        self.promoted.as_deref().unwrap_or(self.backing)
    }

    /// Returns a mutable reference to the current data and marks the account as dirty.
    pub fn data_mut(&mut self) -> &mut [u8] {
        self.dirty = true;
        match self.promoted {
            Some(ref mut v) => v.as_mut_slice(),
            None => self.backing,
        }
    }

    /// Resizes the account data to `new_len` bytes.
    ///
    /// When shrinking, truncates in-place without allocating. When growing, promotes to a
    /// heap-allocated buffer (copies existing data, pads with zeros).
    pub fn resize(&mut self, new_len: usize) {
        let current_len = self.data().len();
        if new_len <= current_len {
            // Shrink: truncate without allocating.
            match self.promoted {
                Some(ref mut v) => v.truncate(new_len),
                None => {
                    let backing = core::mem::take(&mut self.backing);
                    self.backing = &mut backing[..new_len];
                }
            }
        } else {
            // Grow.
            match self.promoted {
                Some(ref mut v) => v.resize(new_len, 0),
                None => {
                    let mut buf = Vec::with_capacity(new_len);
                    buf.extend_from_slice(self.backing);
                    buf.resize(new_len, 0);
                    self.promoted = Some(buf);
                }
            }
        }
        self.dirty = true;
    }

    /// Marks this account for deletion.
    pub fn mark_deleted(&mut self) {
        self.deleted = true;
        self.dirty = true;
    }

    /// Returns `true` if the account data has been modified.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Returns `true` if the account has been marked for deletion.
    pub fn is_deleted(&self) -> bool {
        self.deleted
    }
}

/// Serializes as `Option<StorageOp>`, translating the account's dirty/deleted/new flags into the
/// corresponding storage operation variant. Batches the variant byte and length prefix into a
/// single 5-byte write to minimize I/O calls.
impl BorshSerialize for Account<'_> {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> borsh::io::Result<()> {
        if self.deleted {
            // Some(StorageOp::Delete): Option tag + variant.
            writer.write_all(&[1, StorageOp::DELETE])?;
        } else if self.dirty {
            let data = self.data();
            // Some(Create/Update): Option tag + variant + length prefix in one write.
            let variant = if self.is_new { StorageOp::CREATE } else { StorageOp::UPDATE };
            let mut header = [1u8, 0, 0, 0, 0, 0];
            header[1] = variant;
            header[2..6].copy_from_slice(&(data.len() as u32).to_le_bytes());
            writer.write_all(&header)?;
            writer.write_all(data)?;
        } else {
            // None: account unchanged.
            writer.write_all(&[0])?;
        }
        Ok(())
    }
}
