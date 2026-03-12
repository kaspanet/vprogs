use alloc::vec::Vec;
use core::mem;

use vprogs_core_types::ResourceId;

use crate::{Parser, Result};

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
    /// Returns the resource identifier.
    pub fn id(&self) -> &ResourceId {
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

    /// Resizes data to `new_len` bytes, promoting to a heap-allocated buffer if necessary.
    pub fn resize(&mut self, new_len: usize) {
        if new_len <= self.data().len() {
            // Shrink: truncate in-place without allocating.
            match self.promoted {
                Some(ref mut v) => v.truncate(new_len),
                None => {
                    let backing = mem::take(&mut self.backing);
                    self.backing = &mut backing[..new_len];
                }
            }
        } else {
            // Grow: resize in-place or promote to heap if still on the backing slice.
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

// Wire format internals — resources are managed by the framework, not serialized by guests.
impl<'a> Resource<'a> {
    /// Wire size of a resource header: resource_id(32) + flags(1) + resource_index(4) +
    /// data_len(4).
    pub const HEADER_SIZE: usize = 32 + 1 + 4 + 4;

    /// Decodes a resource from its header bytes, splitting off its backing from `data` and
    /// advancing past the consumed bytes.
    pub(crate) fn decode(mut header: &'a [u8], buf: &mut &'a mut [u8]) -> Result<Self> {
        // Parse header fields.
        let resource_id = header.consume_array::<32>("resource_id")?;
        let is_new = header.consume_bool("is_new")?;
        let resource_index = header.consume_u32("resource_index")?;
        let data_length = header.consume_u32("data_length")? as usize;

        // Split off the backing slice from the start of `data` and advance `data` past it.
        let (backing, rest) = mem::take(buf).split_at_mut(data_length);
        *buf = rest;

        Ok(Self {
            resource_id: ResourceId::from_bytes_ref(resource_id),
            backing,
            promoted: None,
            resource_index,
            is_new,
            dirty: false,
            deleted: false,
        })
    }

    /// Encodes a resource header to the given writer.
    #[cfg(feature = "host")]
    pub(crate) fn encode_header(
        w: &mut impl crate::Write,
        resource_id: &ResourceId,
        is_new: bool,
        resource_index: u32,
        data_len: u32,
    ) {
        // Write header fields.
        w.write(resource_id.as_bytes());
        w.write(&[if is_new { 1 } else { 0 }]);
        w.write(&resource_index.to_le_bytes());
        w.write(&data_len.to_le_bytes());
    }
}
