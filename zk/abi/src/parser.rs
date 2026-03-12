use crate::{Error, Result};

/// Extension trait for parsing wire format fields from byte slices.
///
/// All methods advance the cursor past the consumed bytes.
pub trait Parser<'a> {
    /// Reads a single byte as a boolean (`!= 0`) and advances the cursor past it.
    fn consume_bool(&mut self, field: &'static str) -> Result<bool>;

    /// Reads a single byte and advances the cursor past it.
    fn consume_u8(&mut self, field: &'static str) -> Result<u8>;

    /// Reads a little-endian `u32` and advances the cursor past it.
    fn consume_u32(&mut self, field: &'static str) -> Result<u32>;

    /// Reads a little-endian `u64` and advances the cursor past it.
    fn consume_u64(&mut self, field: &'static str) -> Result<u64>;

    /// Reads `len` bytes and advances the cursor past them.
    fn consume_bytes(&mut self, len: usize, field: &'static str) -> Result<&'a [u8]>;

    /// Reads a fixed-size array reference and advances the cursor past it.
    fn consume_array<const N: usize>(&mut self, field: &'static str) -> Result<&'a [u8; N]>;
}

impl<'a> Parser<'a> for &'a [u8] {
    fn consume_bool(&mut self, field: &'static str) -> Result<bool> {
        self.consume_u8(field).map(|b| b != 0)
    }

    fn consume_u8(&mut self, field: &'static str) -> Result<u8> {
        let val = self.first().copied().ok_or_else(|| Error::Decode(field.into()))?;
        *self = &self[1..];
        Ok(val)
    }

    fn consume_u32(&mut self, field: &'static str) -> Result<u32> {
        let bytes = self.consume_bytes(4, field)?;
        Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn consume_u64(&mut self, field: &'static str) -> Result<u64> {
        let bytes = self.consume_bytes(8, field)?;
        Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn consume_bytes(&mut self, len: usize, field: &'static str) -> Result<&'a [u8]> {
        if self.len() < len {
            return Err(Error::Decode(field.into()));
        }
        let slice = *self;
        let (consumed, rest) = slice.split_at(len);
        *self = rest;
        Ok(consumed)
    }

    fn consume_array<const N: usize>(&mut self, field: &'static str) -> Result<&'a [u8; N]> {
        let bytes = self.consume_bytes(N, field)?;
        bytes.try_into().map_err(|_| Error::Decode(field.into()))
    }
}
