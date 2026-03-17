use alloc::vec::Vec;

use crate::DecodeError;

type Result<T> = core::result::Result<T, DecodeError>;

/// Extension trait for parsing wire format fields from byte slices.
///
/// All methods advance the cursor past the consumed bytes. Implemented for `&'a [u8]` so that
/// `let mut buf = input; buf.consume_u32_le("field")?;` progressively consumes the buffer.
pub trait Parser<'a> {
    /// Reads a single byte as a boolean (`!= 0`) and advances the cursor past it.
    fn consume_bool(&mut self, field: &'static str) -> Result<bool>;

    /// Reads a single byte and advances the cursor past it.
    fn consume_u8(&mut self, field: &'static str) -> Result<u8>;

    /// Reads a little-endian `u16` and advances the cursor past it.
    fn consume_u16_le(&mut self, field: &'static str) -> Result<u16>;

    /// Reads a big-endian `u16` and advances the cursor past it.
    fn consume_u16_be(&mut self, field: &'static str) -> Result<u16>;

    /// Reads a little-endian `u32` and advances the cursor past it.
    fn consume_u32_le(&mut self, field: &'static str) -> Result<u32>;

    /// Reads a big-endian `u32` and advances the cursor past it.
    fn consume_u32_be(&mut self, field: &'static str) -> Result<u32>;

    /// Reads a little-endian `u64` and advances the cursor past it.
    fn consume_u64_le(&mut self, field: &'static str) -> Result<u64>;

    /// Reads a big-endian `u64` and advances the cursor past it.
    fn consume_u64_be(&mut self, field: &'static str) -> Result<u64>;

    /// Reads `len` bytes and advances the cursor past them.
    fn consume_bytes(&mut self, len: usize, field: &'static str) -> Result<&'a [u8]>;

    /// Reads a fixed-size array reference and advances the cursor past it.
    fn consume_array<const N: usize>(&mut self, field: &'static str) -> Result<&'a [u8; N]>;

    /// Reads a length-prefixed byte blob: `len(4 LE) + bytes(len)`.
    fn consume_blob(&mut self, field: &'static str) -> Result<&'a [u8]>;

    /// Reads a length-prefixed UTF-8 string: `len(4 LE) + bytes(len)`.
    fn consume_string(&mut self, field: &'static str) -> Result<&'a str>;

    /// Advances the cursor past `n` bytes without reading them. Returns `&mut Self` for chaining.
    fn skip(&mut self, n: usize, field: &'static str) -> Result<&mut Self>;

    /// Reads a LE `u32` count, then calls `decode_fn` that many times, collecting into a `Vec`.
    fn consume_n<T>(
        &mut self,
        field: &'static str,
        mut decode_fn: impl FnMut(&mut Self) -> Result<T>,
    ) -> Result<Vec<T>> {
        let n = self.consume_u32_le(field)? as usize;
        let mut items = Vec::with_capacity(n);
        for _ in 0..n {
            items.push(decode_fn(self)?);
        }
        Ok(items)
    }
}

impl<'a> Parser<'a> for &'a [u8] {
    fn consume_bool(&mut self, field: &'static str) -> Result<bool> {
        self.consume_u8(field).map(|b| b != 0)
    }

    fn consume_u8(&mut self, field: &'static str) -> Result<u8> {
        let val = self.first().copied().ok_or(DecodeError(field))?;
        *self = &self[1..];
        Ok(val)
    }

    fn consume_u16_le(&mut self, field: &'static str) -> Result<u16> {
        let bytes = self.consume_bytes(2, field)?;
        Ok(u16::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn consume_u16_be(&mut self, field: &'static str) -> Result<u16> {
        let bytes = self.consume_bytes(2, field)?;
        Ok(u16::from_be_bytes(bytes.try_into().unwrap()))
    }

    fn consume_u32_le(&mut self, field: &'static str) -> Result<u32> {
        let bytes = self.consume_bytes(4, field)?;
        Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn consume_u32_be(&mut self, field: &'static str) -> Result<u32> {
        let bytes = self.consume_bytes(4, field)?;
        Ok(u32::from_be_bytes(bytes.try_into().unwrap()))
    }

    fn consume_u64_le(&mut self, field: &'static str) -> Result<u64> {
        let bytes = self.consume_bytes(8, field)?;
        Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn consume_u64_be(&mut self, field: &'static str) -> Result<u64> {
        let bytes = self.consume_bytes(8, field)?;
        Ok(u64::from_be_bytes(bytes.try_into().unwrap()))
    }

    fn consume_bytes(&mut self, len: usize, field: &'static str) -> Result<&'a [u8]> {
        if self.len() < len {
            return Err(DecodeError(field));
        }
        let slice = *self;
        let (consumed, rest) = slice.split_at(len);
        *self = rest;
        Ok(consumed)
    }

    fn consume_array<const N: usize>(&mut self, field: &'static str) -> Result<&'a [u8; N]> {
        let bytes = self.consume_bytes(N, field)?;
        bytes.try_into().map_err(|_| DecodeError(field))
    }

    fn skip(&mut self, n: usize, field: &'static str) -> Result<&mut Self> {
        self.consume_bytes(n, field)?;
        Ok(self)
    }

    fn consume_blob(&mut self, field: &'static str) -> Result<&'a [u8]> {
        let len = self.consume_u32_le(field)? as usize;
        self.consume_bytes(len, field)
    }

    fn consume_string(&mut self, field: &'static str) -> Result<&'a str> {
        let bytes = self.consume_blob(field)?;
        core::str::from_utf8(bytes).map_err(|_| DecodeError(field))
    }
}
