use core::mem::take;

use crate::{Error, Reader, Result};

/// Self-advancing decoder that hands out mutable byte slices.
pub trait MutReader<'a>: Reader<'a> {
    /// Reads `len` raw bytes mutably, advancing the cursor past them.
    fn bytes_mut(&mut self, len: usize, field: &'static str) -> Result<&'a mut [u8]>;

    /// Reads a length-prefixed mutable byte blob: `len(4 LE) + bytes(len)`.
    fn blob_mut(&mut self, field: &'static str) -> Result<&'a mut [u8]> {
        let len = self.le_u32(field)? as usize;
        self.bytes_mut(len, field)
    }
}

impl<'a> MutReader<'a> for &'a mut [u8] {
    fn bytes_mut(&mut self, len: usize, field: &'static str) -> Result<&'a mut [u8]> {
        let (consumed, rest) = take(self).split_at_mut_checked(len).ok_or(Error::Decode(field))?;
        *self = rest;
        Ok(consumed)
    }
}
