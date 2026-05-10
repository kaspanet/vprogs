use alloc::vec::Vec;
use core::mem;

#[cfg(feature = "host")]
use vprogs_core_codec::Writer;
use vprogs_core_codec::{MutReader, Reader};
use vprogs_core_types::{AccessMetadata, AccessType, ResourceId};
#[cfg(feature = "host")]
use vprogs_scheduling_scheduler::{AccessHandle, Processor};
#[cfg(feature = "host")]
use vprogs_storage_types::Store;

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
        let current_len = self.data().len();
        if new_len == current_len {
            return;
        }

        if new_len < current_len {
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

    /// Decodes a resource, splitting off its backing from `buf`.
    pub fn decode(buf: &mut &'a mut [u8], access_metadata: &'a AccessMetadata) -> Result<Self> {
        Ok(Self {
            access_metadata,
            is_new: buf.bool("is_new")?,
            index: buf.le_u32("index")?,
            backing: buf.blob_mut("data")?,
            promoted: None,
            dirty: false,
            deleted: false,
        })
    }

    /// Encodes a single resource to the journal.
    #[cfg(feature = "host")]
    pub fn encode<S, P>(w: &mut impl Writer, r: &AccessHandle<'_, S, P>)
    where
        S: Store,
        P: Processor<S>,
    {
        w.write(&[r.is_new() as u8]);
        w.write(&r.resource_index().to_le_bytes());
        w.write_blob(r.data());
    }
}
