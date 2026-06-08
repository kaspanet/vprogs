use alloc::vec::Vec;
use core::{mem::take, str::from_utf8};

use zerocopy::{Immutable, KnownLayout, TryFromBytes};

use crate::{Error, Result};

/// Self-advancing decoder for wire format fields.
///
/// All methods consume bytes from the front of the slice and advance the cursor past them.
/// Implemented for `&'a [u8]` and `&'a mut [u8]` so callers can write
/// `let mut buf = input; buf.le_u32("field")?;` to progressively decode either an immutable or
/// mutable buffer.
///
/// Implementors only need to provide [`bytes`](Self::bytes), [`array`](Self::array), and
/// [`remaining`](Self::remaining); every other method has a default implementation.
///
/// Every method takes a `field` name for error diagnostics.
pub trait Reader<'a> {
    /// Reads `len` raw bytes, advancing the cursor past them.
    fn bytes(&mut self, len: usize, field: &'static str) -> Result<&'a [u8]>;

    /// Reads a fixed-size array reference, advancing the cursor past it.
    fn array<const N: usize>(&mut self, field: &'static str) -> Result<&'a [u8; N]>;

    /// Returns the number of bytes left in the buffer.
    fn remaining(&self) -> usize;

    /// Reads a single byte.
    fn byte(&mut self, field: &'static str) -> Result<u8> {
        Ok(self.array::<1>(field)?[0])
    }

    /// Reads a single byte as a boolean (`!= 0`).
    fn bool(&mut self, field: &'static str) -> Result<bool> {
        self.byte(field).map(|b| b != 0)
    }

    /// Reads a little-endian `u16`.
    fn le_u16(&mut self, field: &'static str) -> Result<u16> {
        Ok(u16::from_le_bytes(*self.array::<2>(field)?))
    }

    /// Reads a big-endian `u16`.
    fn be_u16(&mut self, field: &'static str) -> Result<u16> {
        Ok(u16::from_be_bytes(*self.array::<2>(field)?))
    }

    /// Reads a little-endian `u32`.
    fn le_u32(&mut self, field: &'static str) -> Result<u32> {
        Ok(u32::from_le_bytes(*self.array::<4>(field)?))
    }

    /// Reads a big-endian `u32`.
    fn be_u32(&mut self, field: &'static str) -> Result<u32> {
        Ok(u32::from_be_bytes(*self.array::<4>(field)?))
    }

    /// Reads a little-endian `u64`.
    fn le_u64(&mut self, field: &'static str) -> Result<u64> {
        Ok(u64::from_le_bytes(*self.array::<8>(field)?))
    }

    /// Reads a big-endian `u64`.
    fn be_u64(&mut self, field: &'static str) -> Result<u64> {
        Ok(u64::from_be_bytes(*self.array::<8>(field)?))
    }

    /// Reads `size_of::<T>()` bytes and casts them to a `&T`.
    fn array_as<T: TryFromBytes + KnownLayout + Immutable>(
        &mut self,
        field: &'static str,
    ) -> Result<&'a T> {
        Ok(T::try_ref_from_bytes(self.bytes(size_of::<T>(), field)?)?)
    }

    /// Reads a u32 LE count, then `count * size_of::<T>()` bytes cast to a `&[T]`.
    fn slice_as<T: TryFromBytes + KnownLayout + Immutable>(
        &mut self,
        field: &'static str,
    ) -> Result<&'a [T]> {
        let count = self.le_u32(field)? as usize;
        let len = count.checked_mul(size_of::<T>()).ok_or(Error::Decode(field))?;
        Ok(<[T]>::try_ref_from_bytes(self.bytes(len, field)?)?)
    }

    /// Reads a length-prefixed byte blob: `len(4 LE) + bytes(len)`.
    fn blob(&mut self, field: &'static str) -> Result<&'a [u8]> {
        let len = self.le_u32(field)? as usize;
        self.bytes(len, field)
    }

    /// Reads a length-prefixed byte blob (`len(4 LE) + bytes(len)`) and casts it to a `&T`.
    fn blob_as<T: ?Sized + TryFromBytes + KnownLayout + Immutable>(
        &mut self,
        field: &'static str,
    ) -> Result<&'a T> {
        Ok(T::try_ref_from_bytes(self.blob(field)?)?)
    }

    /// Reads a length-prefixed UTF-8 string: `len(4 LE) + bytes(len)`.
    fn string(&mut self, field: &'static str) -> Result<&'a str> {
        from_utf8(self.blob(field)?).map_err(|_| Error::Decode(field))
    }

    /// Advances the cursor past `n` bytes without returning them. Returns `&mut Self` for chaining.
    fn skip(&mut self, n: usize, field: &'static str) -> Result<&mut Self> {
        self.bytes(n, field)?;
        Ok(self)
    }

    /// Reads a LE `u32` count, then calls `decode_fn` that many times, collecting into a `Vec`.
    ///
    /// Pre-allocation is capped by remaining buffer length to prevent OOM from untrusted counts.
    /// For fixed-layout `T: TryFromBytes + KnownLayout + Immutable`, prefer
    /// [`slice_as`](Self::slice_as): zero-copy and avoids the per-element closure call.
    fn many<T>(
        &mut self,
        field: &'static str,
        mut decode_fn: impl FnMut(&mut Self) -> Result<T>,
    ) -> Result<Vec<T>> {
        let n = self.le_u32(field)? as usize;
        let mut items = Vec::with_capacity(n.min(self.remaining()));
        for _ in 0..n {
            items.push(decode_fn(self)?);
        }
        Ok(items)
    }
}

impl<'a> Reader<'a> for &'a [u8] {
    fn bytes(&mut self, len: usize, field: &'static str) -> Result<&'a [u8]> {
        let (consumed, rest) = self.split_at_checked(len).ok_or(Error::Decode(field))?;
        *self = rest;
        Ok(consumed)
    }

    fn array<const N: usize>(&mut self, field: &'static str) -> Result<&'a [u8; N]> {
        let (chunk, rest) = self.split_first_chunk::<N>().ok_or(Error::Decode(field))?;
        *self = rest;
        Ok(chunk)
    }

    fn remaining(&self) -> usize {
        self.len()
    }
}

impl<'a> Reader<'a> for &'a mut [u8] {
    fn bytes(&mut self, len: usize, field: &'static str) -> Result<&'a [u8]> {
        let (consumed, rest) = take(self).split_at_mut_checked(len).ok_or(Error::Decode(field))?;
        *self = rest;
        Ok(consumed)
    }

    fn array<const N: usize>(&mut self, field: &'static str) -> Result<&'a [u8; N]> {
        let (chunk, rest) = take(self).split_first_chunk_mut::<N>().ok_or(Error::Decode(field))?;
        *self = rest;
        Ok(chunk)
    }

    fn remaining(&self) -> usize {
        self.len()
    }
}
