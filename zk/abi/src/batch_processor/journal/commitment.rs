use crate::{Error, Write};

/// Journal commitment for a batch proof — success or error.
///
/// Wire format: `discriminant(1) + payload`. Success payload is `prev_root(32) + new_root(32) +
/// batch_index(8)`. Error payload is the encoded `Error`.
pub struct JournalCommitment;

impl JournalCommitment {
    /// Wire discriminant for a successful batch.
    const SUCCESS: u8 = 0x00;
    /// Wire discriminant for a failed batch.
    const ERROR: u8 = 0x01;

    /// Encodes the batch result to the journal (guest-side).
    pub fn encode(w: &mut impl Write, result: &Result<([u8; 32], [u8; 32], u64), Error>) {
        match result {
            Ok((prev_root, new_root, batch_index)) => {
                w.write(&[Self::SUCCESS]);
                w.write(prev_root);
                w.write(new_root);
                w.write(&batch_index.to_le_bytes());
            }
            Err(error) => {
                w.write(&[Self::ERROR]);
                error.encode(w);
            }
        }
    }

    /// Decodes the journal commitment from a batch proof receipt (host-side).
    ///
    /// Returns `Ok((prev_root, new_root, batch_index))` on success, or the guest error.
    #[cfg(feature = "host")]
    pub fn decode(buf: &[u8]) -> crate::Result<([u8; 32], [u8; 32], u64)> {
        use vprogs_core_codec::Reader;

        let mut buf = buf;
        match buf.byte("discriminant")? {
            Self::SUCCESS => {
                let prev_root = *buf.array::<32>("prev_root")?;
                let new_root = *buf.array::<32>("new_root")?;
                let batch_index = buf.le_u64("batch_index")?;
                Ok((prev_root, new_root, batch_index))
            }
            Self::ERROR => Err(Error::decode(&mut buf)?),
            _ => Err(Error::Decode("invalid journal commitment discriminant".into())),
        }
    }
}
