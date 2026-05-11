use core::mem::take;

use vprogs_core_codec::{Reader, Writer};

use crate::{
    Error, Result,
    transaction_processor::{OutputResourceCommitment, OutputResourceCommitments, Resource},
};

/// Decoded output commitment from a transaction processor journal.
pub enum OutputCommitment<'a> {
    /// Transaction executed successfully.
    Success(OutputResourceCommitments<'a>),
    /// Transaction execution failed.
    Error(Error),
}

impl<'a> OutputCommitment<'a> {
    /// Wire discriminant for a successful execution.
    pub const SUCCESS: u8 = 0x00;
    /// Wire discriminant for a failed execution.
    pub const ERROR: u8 = 0x01;

    /// Wire size of the encoded output commitment payload.
    pub fn wire_size(result: &Result<&[Resource<'_>]>) -> usize {
        // discriminant(1) + variant body.
        1 + match result {
            Ok(resources) => {
                resources.iter().map(OutputResourceCommitment::wire_size).sum::<usize>()
            }
            Err(err) => err.wire_size(),
        }
    }

    /// Decodes an output commitment, advancing `buf` past the consumed bytes.
    pub fn decode(buf: &mut &'a [u8]) -> Result<Self> {
        match buf.byte("discriminant")? {
            Self::SUCCESS => Ok(Self::Success(OutputResourceCommitments::new(take(buf)))),
            Self::ERROR => Ok(Self::Error(Error::decode(buf)?)),
            _ => Err(Error::Decode("invalid output commitment discriminant".into())),
        }
    }

    /// Encodes an output commitment payload to the journal.
    pub fn encode(w: &mut impl Writer, result: &Result<&[Resource<'_>]>) {
        match *result {
            Ok(resources) => {
                w.write(&[Self::SUCCESS]);
                for r in resources {
                    OutputResourceCommitment::encode(w, r);
                }
            }
            Err(ref err) => {
                w.write(&[Self::ERROR]);
                err.encode(w);
            }
        }
    }
}
