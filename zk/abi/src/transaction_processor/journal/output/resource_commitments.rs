use crate::transaction_processor::OutputResourceCommitment;

/// Zero-copy iterator over per-resource output hash commitments.
pub struct OutputResourceCommitments<'a> {
    /// Remaining unconsumed bytes of the commitment entries.
    buf: &'a [u8],
}

impl<'a> OutputResourceCommitments<'a> {
    /// Creates a new iterator over variable-size output commitment entries in `buf`.
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }
}

impl<'a> Iterator for OutputResourceCommitments<'a> {
    type Item = OutputResourceCommitment<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        // Check if all entries have been consumed.
        if self.buf.is_empty() {
            return None;
        }

        // Decode next entry, advancing the buffer.
        Some(OutputResourceCommitment::decode(&mut self.buf).expect("pre-validated payload"))
    }
}
