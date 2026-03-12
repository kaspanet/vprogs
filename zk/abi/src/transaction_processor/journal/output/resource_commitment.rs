use crate::{Error, Parser, Result};

/// A single resource's output commitment.
pub enum OutputResourceCommitment<'a> {
    /// Resource data was modified; contains the new data hash.
    Changed(&'a [u8; 32]),
    /// Resource data was not modified.
    Unchanged,
}

impl<'a> OutputResourceCommitment<'a> {
    /// Wire flag: resource data was modified (32-byte hash follows).
    pub const CHANGED: u8 = 0x01;
    /// Wire flag: resource data was not modified (no hash follows).
    pub const UNCHANGED: u8 = 0x00;

    /// Decodes a single output commitment, advancing `buf` past the consumed bytes.
    pub fn decode(buf: &mut &'a [u8]) -> Result<Self> {
        // Parse flag byte.
        let flag = buf[0];
        *buf = &buf[1..];

        // Decode variant based on flag.
        match flag {
            Self::CHANGED => {
                let hash: &[u8; 32] = buf[..32].parse_into("hash")?;
                *buf = &buf[32..];

                Ok(Self::Changed(hash))
            }
            Self::UNCHANGED => Ok(Self::Unchanged),
            _ => Err(Error::Decode("invalid resource output flag".into())),
        }
    }
}
