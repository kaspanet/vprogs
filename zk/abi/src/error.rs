use alloc::string::String;

use crate::{Parser, Write};

/// Errors from ZK ABI operations.
#[derive(Clone, Debug, thiserror::Error)]
pub enum Error {
    /// Error code returned by the guest program.
    #[error("guest error: code {0}")]
    Guest(u32),
    /// Wire format decode error.
    #[error("decode error: {0}")]
    Decode(String),
}

impl Error {
    /// Wire discriminant for a guest error.
    const GUEST: u8 = 0x00;
    /// Wire discriminant for a decode error.
    const DECODE: u8 = 0x01;

    /// Returns the wire size of the encoded error.
    pub fn wire_size(&self) -> usize {
        match self {
            // discriminant(1) + code(4).
            Self::Guest(_) => 1 + 4,
            // discriminant(1) + msg_len(4) + msg(N).
            Self::Decode(msg) => 1 + 4 + msg.len(),
        }
    }

    /// Decodes an error, advancing `buf` past the consumed bytes.
    pub fn decode(buf: &mut &[u8]) -> Result<Self> {
        // Dispatch based on discriminant.
        match buf.consume_u8("error_variant")? {
            Self::GUEST => Ok(Self::Guest(buf.consume_u32("error_code")?)),
            Self::DECODE => Ok(Self::Decode(buf.consume_string("error_msg")?.into())),
            _ => Err(Error::Decode("invalid error discriminant".into())),
        }
    }

    /// Encodes the error to the given writer.
    pub fn encode(&self, w: &mut impl Write) {
        match self {
            Self::Guest(code) => {
                // Write discriminant and code.
                w.write(&[Self::GUEST]);
                w.write(&code.to_le_bytes());
            }
            Self::Decode(msg) => {
                // Write discriminant, length, and UTF-8 message bytes.
                w.write(&[Self::DECODE]);
                w.write(&(msg.len() as u32).to_le_bytes());
                w.write(msg.as_bytes());
            }
        }
    }
}

/// Result type for ZK ABI operations.
pub type Result<T> = core::result::Result<T, Error>;
