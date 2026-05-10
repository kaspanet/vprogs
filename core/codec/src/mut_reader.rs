use core::mem::take;

use crate::{Error, Reader, Result};

/// Self-advancing decoder that hands out mutable byte slices.
pub trait MutReader<'a>: Reader<'a> {
    /// Reads `len` raw bytes mutably, advancing the cursor past them.
    fn bytes_mut(&mut self, len: usize, field: &'static str) -> Result<&'a mut [u8]>;
}

impl<'a> MutReader<'a> for &'a mut [u8] {
    fn bytes_mut(&mut self, len: usize, field: &'static str) -> Result<&'a mut [u8]> {
        let (consumed, rest) = take(self).split_at_mut_checked(len).ok_or(Error::Decode(field))?;
        *self = rest;
        Ok(consumed)
    }
}
