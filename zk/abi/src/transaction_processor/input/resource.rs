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
    /// Whether the slot held no committed data when this transaction started.
    is_new: bool,
    /// Whether the resource data has been modified.
    dirty: bool,
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

    /// Returns `true` if the slot held no committed data when this transaction started. Writing
    /// data does not clear it.
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

    /// Returns `true` if the resource data has been modified.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Decodes a resource, splitting off its backing from `buf`.
    pub fn decode(buf: &mut &'a mut [u8], access_metadata: &'a AccessMetadata) -> Result<Self> {
        let index = buf.le_u32("index")?;
        let backing = buf.blob_mut("data")?;

        // Newness is derived, never read from the wire. An empty slot commits `EMPTY_HASH`, the
        // same value hash an absent key proves against, so the journal's data-hash check against
        // the old root already binds it. A wire flag would be unbound and freely forgeable.
        let is_new = backing.is_empty();

        Ok(Self { access_metadata, index, is_new, backing, promoted: None, dirty: false })
    }

    /// Encodes a single resource to the journal.
    #[cfg(feature = "host")]
    pub fn encode<S, P>(w: &mut impl Writer, r: &AccessHandle<'_, S, P>)
    where
        S: Store,
        P: Processor<S>,
    {
        w.write(&r.resource_index().to_le_bytes());
        w.write_blob(r.data());
    }
}

#[cfg(test)]
mod tests {
    use vprogs_core_types::{AccessMetadata, AccessType, ResourceId};

    use super::*;

    fn access_metadata() -> AccessMetadata {
        AccessMetadata { resource_id: ResourceId::from([7u8; 32]), access_type: AccessType::Write }
    }

    fn wire(index: u32, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&index.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
        out
    }

    /// An empty slot is the absent-key state the old root proves against, so it decodes as new.
    #[test]
    fn empty_data_decodes_as_new() {
        let meta = access_metadata();
        let mut buf = wire(3, &[]);
        let r = Resource::decode(&mut buf.as_mut_slice(), &meta).unwrap();
        assert!(r.is_new());
        assert_eq!(r.index(), 3);
    }

    /// Committed bytes pin the slot to live: there is no wire flag that could present them as new.
    #[test]
    fn non_empty_data_decodes_as_live() {
        let meta = access_metadata();
        let mut buf = wire(0, &[1, 2, 3]);
        let r = Resource::decode(&mut buf.as_mut_slice(), &meta).unwrap();
        assert!(!r.is_new());
        assert_eq!(r.data(), &[1, 2, 3]);
    }

    /// Newness is a decode-time snapshot: writing data does not retroactively make the slot live,
    /// so an action that created a slot still sees the creation it performed.
    #[test]
    fn is_new_is_a_snapshot_and_survives_writes() {
        let meta = access_metadata();
        let mut buf = wire(0, &[]);
        let mut r = Resource::decode(&mut buf.as_mut_slice(), &meta).unwrap();
        assert!(r.is_new());

        r.resize(4);
        r.data_mut().copy_from_slice(&[9, 9, 9, 9]);
        assert!(r.is_new(), "is_new must not flip once the slot is written");
    }
}
