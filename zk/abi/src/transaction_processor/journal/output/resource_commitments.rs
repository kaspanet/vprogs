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
        if self.buf.is_empty() {
            return None;
        }

        let flag = self.buf[0];
        self.buf = &self.buf[1..];

        match flag {
            OutputResourceCommitment::CHANGED => {
                let hash: &[u8; 32] = self.buf[..32].try_into().unwrap();
                self.buf = &self.buf[32..];
                Some(OutputResourceCommitment::Changed(hash))
            }
            OutputResourceCommitment::UNCHANGED => Some(OutputResourceCommitment::Unchanged),
            _ => panic!("invalid resource output flag: {flag}"),
        }
    }
}
