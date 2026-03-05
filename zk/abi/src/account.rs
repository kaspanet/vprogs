use alloc::vec::Vec;

use vprogs_core_types::ResourceId;

/// A mutable view of a single account's data within a decoded witness buffer.
///
/// Data starts as a borrowed slice into the wire buffer (`backing`). If the caller needs more
/// space than the original slice provides, [`grow`](Account::grow) promotes to a
/// heap-allocated `Vec` (`promoted`). Reads and writes always go through the active buffer.
pub struct Account<'a> {
    resource_id: ResourceId,
    is_new: bool,
    backing: &'a mut [u8],
    promoted: Option<Vec<u8>>,
    dirty: bool,
    deleted: bool,
}

impl<'a> Account<'a> {
    /// Creates a new `Account` borrowing into the given slice.
    pub fn new(resource_id: ResourceId, is_new: bool, backing: &'a mut [u8]) -> Self {
        Self { resource_id, is_new, backing, promoted: None, dirty: false, deleted: false }
    }

    pub fn resource_id(&self) -> &ResourceId {
        &self.resource_id
    }

    pub fn is_new(&self) -> bool {
        self.is_new
    }

    /// Returns a shared reference to the current data (backing or promoted).
    pub fn data(&self) -> &[u8] {
        self.promoted.as_deref().unwrap_or(self.backing)
    }

    /// Returns a mutable reference to the current data (backing or promoted) and marks dirty.
    pub fn data_mut(&mut self) -> &mut [u8] {
        self.dirty = true;
        match self.promoted {
            Some(ref mut v) => v.as_mut_slice(),
            None => self.backing,
        }
    }

    /// Promotes to a heap-allocated buffer of `new_len` bytes (copies existing data, pads with
    /// zeros). Subsequent borrows use the promoted buffer.
    pub fn grow(&mut self, new_len: usize) {
        let src = self.promoted.as_deref().unwrap_or(self.backing);
        let mut buf = Vec::with_capacity(new_len);
        let copy_len = src.len().min(new_len);
        buf.extend_from_slice(&src[..copy_len]);
        buf.resize(new_len, 0);
        self.promoted = Some(buf);
        self.dirty = true;
    }

    pub fn mark_deleted(&mut self) {
        self.deleted = true;
        self.dirty = true;
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn is_deleted(&self) -> bool {
        self.deleted
    }
}
