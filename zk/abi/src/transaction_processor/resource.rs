use alloc::vec::Vec;

use borsh::{
    BorshSerialize,
    io::{self, Write},
};
use vprogs_core_types::ResourceId;

use super::StorageOp;

/// A mutable view of a single resource's data within a decoded wire buffer.
///
/// Data starts as a borrowed slice into the wire buffer (`backing`). If the caller needs more space
/// than the original slice provides, [`resize`](Resource::resize) promotes to a heap-allocated
/// `Vec` (`promoted`). Reads and writes always go through the active buffer.
pub struct Resource<'a> {
    /// Zero-copy reference into the wire buffer's resource header.
    resource_id: &'a ResourceId,
    /// Zero-copy mutable slice into the wire buffer's payload region.
    backing: &'a mut [u8],
    /// Heap-allocated buffer, used only when the resource data outgrows `backing`.
    promoted: Option<Vec<u8>>,
    /// Per-batch resource index assigned when the resource is first accessed.
    resource_index: u32,
    /// Whether this resource was created by the current transaction.
    is_new: bool,
    /// Whether the resource data has been modified.
    dirty: bool,
    /// Whether the resource has been marked for deletion.
    deleted: bool,
}

impl<'a> Resource<'a> {
    /// Creates a new `Resource` borrowing into the given slice.
    pub fn new(
        resource_id: &'a ResourceId,
        resource_index: u32,
        is_new: bool,
        backing: &'a mut [u8],
    ) -> Self {
        Self {
            resource_id,
            backing,
            promoted: None,
            resource_index,
            is_new,
            dirty: false,
            deleted: false,
        }
    }

    /// Returns the resource identifier.
    pub fn resource_id(&self) -> &ResourceId {
        self.resource_id
    }

    /// Returns `true` if this resource was created by the current transaction.
    pub fn is_new(&self) -> bool {
        self.is_new
    }

    /// Returns the per-batch resource index.
    pub fn index(&self) -> u32 {
        self.resource_index
    }

    /// Returns a shared reference to the current data (backing or promoted).
    pub fn data(&self) -> &[u8] {
        self.promoted.as_deref().unwrap_or(self.backing)
    }

    /// Returns a mutable reference to the current data and marks the resource as dirty.
    pub fn data_mut(&mut self) -> &mut [u8] {
        self.dirty = true;
        match self.promoted {
            Some(ref mut v) => v.as_mut_slice(),
            None => self.backing,
        }
    }

    /// Resizes the resource data to `new_len` bytes.
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

    /// Marks this resource for deletion.
    pub fn mark_deleted(&mut self) {
        self.deleted = true;
        self.dirty = true;
    }

    /// Returns `true` if the resource data has been modified.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Returns `true` if the resource has been marked for deletion.
    pub fn is_deleted(&self) -> bool {
        self.deleted
    }
}

/// Serializes as `Option<StorageOp>`, translating the resource's dirty/deleted/new flags into the
/// corresponding storage operation variant. Batches the variant byte and length prefix into a
/// single 5-byte write to minimize I/O calls.
impl BorshSerialize for Resource<'_> {
    fn serialize<W: Write>(&self, writer: &mut W) -> io::Result<()> {
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
            // None: resource unchanged.
            writer.write_all(&[0])?;
        }
        Ok(())
    }
}
