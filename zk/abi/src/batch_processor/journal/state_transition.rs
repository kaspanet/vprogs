use crate::{Error, Result, Write};

/// Proven state transition for a batch — success or error (zero-copy on decode).
///
/// Wire format: `discriminant(1) + payload`. Success payload is `image_id(32) + prev_root(32) +
/// new_root(32)`. Error payload is the encoded `Error`.
pub enum StateTransition<'a> {
    /// Batch verified successfully.
    Success {
        /// Transaction processor guest image ID that was verified.
        image_id: &'a [u8; 32],
        /// State root before this batch was applied.
        prev_root: &'a [u8; 32],
        /// State root after this batch was applied.
        new_root: &'a [u8; 32],
    },
    /// Batch verification failed.
    Error(Error),
}

impl<'a> StateTransition<'a> {
    /// Wire discriminant for a successful batch.
    const SUCCESS: u8 = 0x00;
    /// Wire discriminant for a failed batch.
    const ERROR: u8 = 0x01;

    /// Encodes a batch result to the journal (guest-side).
    pub fn encode(w: &mut impl Write, result: &Result<(&[u8; 32], [u8; 32], [u8; 32])>) {
        match result {
            Ok((image_id, prev_root, new_root)) => {
                w.write(&[Self::SUCCESS]);
                w.write(*image_id);
                w.write(prev_root);
                w.write(new_root);
            }
            Err(error) => {
                w.write(&[Self::ERROR]);
                error.encode(w);
            }
        }
    }

    /// Decodes a state transition from a batch proof receipt (host-side, zero-copy).
    #[cfg(feature = "host")]
    pub fn decode(buf: &'a [u8]) -> Result<Self> {
        use vprogs_core_codec::Reader;

        let mut buf = buf;
        match buf.byte("discriminant")? {
            Self::SUCCESS => Ok(Self::Success {
                image_id: buf.array::<32>("image_id")?,
                prev_root: buf.array::<32>("prev_root")?,
                new_root: buf.array::<32>("new_root")?,
            }),
            Self::ERROR => Ok(Self::Error(Error::decode(&mut buf)?)),
            _ => Err(Error::Decode("invalid state transition discriminant".into())),
        }
    }
}
