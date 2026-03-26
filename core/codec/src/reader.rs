use alloc::vec::Vec;

use crate::{Error, Result};

/// Self-advancing decoder for wire format fields.
///
/// All methods consume bytes from the front of the slice and advance the cursor past them.
/// Implemented for `&'a [u8]` so callers write `let mut buf = input; buf.le_u32("field")?;` to
/// progressively decode a buffer.
///
/// Every method takes a `field` name for error diagnostics.
pub trait Reader<'a> {
    /// Reads a single byte as a boolean (`!= 0`).
    fn bool(&mut self, field: &'static str) -> Result<bool>;

    /// Reads a single byte.
    fn byte(&mut self, field: &'static str) -> Result<u8>;

    /// Reads a little-endian `u16`.
    fn le_u16(&mut self, field: &'static str) -> Result<u16>;

    /// Reads a big-endian `u16`.
    fn be_u16(&mut self, field: &'static str) -> Result<u16>;

    /// Reads a little-endian `u32`.
    fn le_u32(&mut self, field: &'static str) -> Result<u32>;

    /// Reads a big-endian `u32`.
    fn be_u32(&mut self, field: &'static str) -> Result<u32>;

    /// Reads a little-endian `u64`.
    fn le_u64(&mut self, field: &'static str) -> Result<u64>;

    /// Reads a big-endian `u64`.
    fn be_u64(&mut self, field: &'static str) -> Result<u64>;

    /// Reads `len` raw bytes.
    fn bytes(&mut self, len: usize, field: &'static str) -> Result<&'a [u8]>;

    /// Reads a fixed-size array reference.
    fn array<const N: usize>(&mut self, field: &'static str) -> Result<&'a [u8; N]>;

    /// Reads a length-prefixed byte blob: `len(4 LE) + bytes(len)`.
    fn blob(&mut self, field: &'static str) -> Result<&'a [u8]>;

    /// Reads a length-prefixed UTF-8 string: `len(4 LE) + bytes(len)`.
    fn string(&mut self, field: &'static str) -> Result<&'a str>;

    /// Advances the cursor past `n` bytes without returning them. Returns `&mut Self` for chaining.
    fn skip(&mut self, n: usize, field: &'static str) -> Result<&mut Self>;

    /// Reads a LE `u32` count, then calls `decode_fn` that many times, collecting into a `Vec`.
    ///
    /// Pre-allocation is capped by remaining buffer length to prevent OOM from untrusted counts.
    fn many<T>(
        &mut self,
        field: &'static str,
        decode_fn: impl FnMut(&mut Self) -> Result<T>,
    ) -> Result<Vec<T>>;
}

impl<'a> Reader<'a> for &'a [u8] {
    fn bool(&mut self, field: &'static str) -> Result<bool> {
        self.byte(field).map(|b| b != 0)
    }

    fn byte(&mut self, field: &'static str) -> Result<u8> {
        let val = self.first().copied().ok_or(Error::Decode(field))?;
        *self = &self[1..];
        Ok(val)
    }

    fn le_u16(&mut self, field: &'static str) -> Result<u16> {
        let bytes = self.bytes(2, field)?;
        Ok(u16::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn be_u16(&mut self, field: &'static str) -> Result<u16> {
        let bytes = self.bytes(2, field)?;
        Ok(u16::from_be_bytes(bytes.try_into().unwrap()))
    }

    fn le_u32(&mut self, field: &'static str) -> Result<u32> {
        let bytes = self.bytes(4, field)?;
        Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn be_u32(&mut self, field: &'static str) -> Result<u32> {
        let bytes = self.bytes(4, field)?;
        Ok(u32::from_be_bytes(bytes.try_into().unwrap()))
    }

    fn le_u64(&mut self, field: &'static str) -> Result<u64> {
        let bytes = self.bytes(8, field)?;
        Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn be_u64(&mut self, field: &'static str) -> Result<u64> {
        let bytes = self.bytes(8, field)?;
        Ok(u64::from_be_bytes(bytes.try_into().unwrap()))
    }

    fn bytes(&mut self, len: usize, field: &'static str) -> Result<&'a [u8]> {
        if self.len() < len {
            return Err(Error::Decode(field));
        }
        let (consumed, rest) = (*self).split_at(len);
        *self = rest;
        Ok(consumed)
    }

    fn array<const N: usize>(&mut self, field: &'static str) -> Result<&'a [u8; N]> {
        let bytes = self.bytes(N, field)?;
        bytes.try_into().map_err(|_| Error::Decode(field))
    }

    fn blob(&mut self, field: &'static str) -> Result<&'a [u8]> {
        let len = self.le_u32(field)? as usize;
        self.bytes(len, field)
    }

    fn string(&mut self, field: &'static str) -> Result<&'a str> {
        let bytes = self.blob(field)?;
        core::str::from_utf8(bytes).map_err(|_| Error::Decode(field))
    }

    fn skip(&mut self, n: usize, field: &'static str) -> Result<&mut Self> {
        self.bytes(n, field)?;
        Ok(self)
    }

    fn many<T>(
        &mut self,
        field: &'static str,
        mut decode_fn: impl FnMut(&mut Self) -> Result<T>,
    ) -> Result<Vec<T>> {
        let n = self.le_u32(field)? as usize;
        let mut items = Vec::with_capacity(n.min(self.len()));
        for _ in 0..n {
            items.push(decode_fn(self)?);
        }
        Ok(items)
    }
}
