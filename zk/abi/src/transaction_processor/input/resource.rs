use alloc::vec::Vec;
use core::mem;

use vprogs_core_codec::Reader;
#[cfg(feature = "host")]
use vprogs_core_codec::Writer;
use vprogs_core_types::{AccessMetadata, AccessType, ResourceId};

use crate::Result;

/// A mutable view of a single resource's data within a decoded wire buffer.
///
/// Data starts as a borrowed slice into the wire buffer (`backing`). If the caller needs more space
/// than the original slice provides, [`resize`](Resource::resize) promotes to a heap-allocated
/// `Vec` (`promoted`). Reads and writes always go through the active buffer.
pub struct Resource<'a> {
    /// Access metadata declared for this resource.
    access_metadata: &'a AccessMetadata,
    /// Zero-copy mutable slice into the wire buffer's payload region.
    backing: &'a mut [u8],
    /// Heap-allocated buffer, used only when the resource data outgrows `backing`.
    promoted: Option<Vec<u8>>,
    /// Per-batch resource index assigned when the resource is first accessed.
    index: u32,
    /// Whether this resource was created by the current transaction.
    is_new: bool,
    /// Whether the resource data has been modified.
    dirty: bool,
    /// Whether the resource has been marked for deletion.
    deleted: bool,
}

impl<'a> Resource<'a> {
    /// Returns the resource identifier.
    pub fn id(&self) -> &'a ResourceId {
        &self.access_metadata.resource_id
    }

    /// Returns the declared access type (Read or Write) for this resource.
    pub fn access_type(&self) -> AccessType {
        self.access_metadata.access_type
    }

    /// Returns `true` if this resource was created by the current transaction.
    pub fn is_new(&self) -> bool {
        self.is_new
    }

    /// Returns the per-batch resource index.
    pub fn index(&self) -> u32 {
        self.index
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

// Wire format internals - resources are managed by the framework, not serialized by guests.
impl<'a> Resource<'a> {
    /// Wire size of a resource header: flags(1) + index(4) + data_len(4).
    pub const HEADER_SIZE: usize = 1 + 4 + 4;

    /// Decodes a resource from its header bytes, splitting off its backing from `data` and
    /// advancing past the consumed bytes.
    pub(crate) fn decode(
        mut header: &'a [u8],
        access_metadata: &'a AccessMetadata,
        buf: &mut &'a mut [u8],
    ) -> Result<Self> {
        // Parse header fields.
        let is_new = header.bool("is_new")?;
        let index = header.le_u32("index")?;
        let data_length = header.le_u32("data_length")? as usize;

        // Split off the backing slice from the start of `data` and advance `data` past it.
        let (backing, rest) = mem::take(buf).split_at_mut(data_length);
        *buf = rest;

        Ok(Self {
            access_metadata,
            backing,
            promoted: None,
            index,
            is_new,
            dirty: false,
            deleted: false,
        })
    }

    /// Encodes a resource header to the given writer.
    #[cfg(feature = "host")]
    pub(crate) fn encode_header(w: &mut impl Writer, is_new: bool, index: u32, data_len: u32) {
        // Write header fields.
        w.write(&[if is_new { 1 } else { 0 }]);
        w.write(&index.to_le_bytes());
        w.write(&data_len.to_le_bytes());
    }
}
