use crate::{Error, Write};

/// Journal commitment for a batch proof.
///
/// Wire format: `discriminant(1) + payload`. Success carries `prev_root(32) + new_root(32) +
/// batch_index(8)`. Error carries the encoded `Error`.
pub struct JournalCommitment;

impl JournalCommitment {
    /// Wire discriminant for a successful batch.
    const SUCCESS: u8 = 0x00;
    /// Wire discriminant for a failed batch.
    const ERROR: u8 = 0x01;

    /// Encodes a successful batch commitment (guest-side).
    pub fn encode_success(
        w: &mut impl Write,
        prev_root: &[u8; 32],
        new_root: &[u8; 32],
        batch_index: u64,
    ) {
        w.write(&[Self::SUCCESS]);
        w.write(prev_root);
        w.write(new_root);
        w.write(&batch_index.to_le_bytes());
    }

    /// Encodes a failed batch commitment (guest-side).
    pub fn encode_error(w: &mut impl Write, error: &Error) {
        w.write(&[Self::ERROR]);
        error.encode(w);
    }

    /// Decodes the journal commitment from a batch proof journal (host-side).
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
