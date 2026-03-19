use crate::Write;

/// Journal commitment for a batch proof: `prev_root(32) + new_root(32) + batch_index(8)`.
pub struct JournalCommitment;

impl JournalCommitment {
    /// Total wire size in bytes.
    pub const SIZE: usize = 32 + 32 + 8;

    /// Encodes the journal commitment (guest-side).
    pub fn encode(w: &mut impl Write, prev_root: &[u8; 32], new_root: &[u8; 32], batch_index: u64) {
        w.write(prev_root);
        w.write(new_root);
        w.write(&batch_index.to_le_bytes());
    }

    /// Decodes the journal commitment from a batch proof journal (host-side).
    #[cfg(feature = "host")]
    pub fn decode(buf: &[u8]) -> crate::Result<([u8; 32], [u8; 32], u64)> {
        use vprogs_core_codec::Reader;

        use crate::Error;

        if buf.len() < Self::SIZE {
            return Err(Error::Decode("journal commitment too short".into()));
        }
        let mut buf = &buf[..Self::SIZE];
        let prev_root = *buf.array::<32>("prev_root")?;
        let new_root = *buf.array::<32>("new_root")?;
        let batch_index = buf.le_u64("batch_index")?;
        Ok((prev_root, new_root, batch_index))
    }
}
