use crate::{Error, Result};

/// Extension trait for parsing wire format fields from byte slices.
pub trait Parser {
    /// Parses a fixed-size value from this byte slice.
    fn parse_into<T>(self, field: &'static str) -> Result<T>
    where
        Self: TryInto<T>;

    /// Reads a little-endian `u32` from this byte slice.
    fn parse_u32(self, field: &'static str) -> Result<u32>;

    /// Reads a little-endian `u64` from this byte slice.
    fn parse_u64(self, field: &'static str) -> Result<u64>;
}

impl Parser for &[u8] {
    fn parse_into<T>(self, field: &'static str) -> Result<T>
    where
        Self: TryInto<T>,
    {
        self.try_into().map_err(|_| Error::Decode(field))
    }

    fn parse_u32(self, field: &'static str) -> Result<u32> {
        Ok(u32::from_le_bytes(self.parse_into(field)?))
    }

    fn parse_u64(self, field: &'static str) -> Result<u64> {
        Ok(u64::from_le_bytes(self.parse_into(field)?))
    }
}
