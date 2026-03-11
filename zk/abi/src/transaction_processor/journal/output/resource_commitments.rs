use crate::transaction_processor::OutputResourceCommitment;

/// Zero-copy iterator over per-resource output hash commitments.
pub struct OutputResourceCommitments<'a> {
    pub buf: &'a [u8],
    pub offset: usize,
}

impl<'a> Iterator for OutputResourceCommitments<'a> {
    type Item = OutputResourceCommitment<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.buf.len() {
            return None;
        }

        let flag = self.buf[self.offset];
        self.offset += 1;

        match flag {
            OutputResourceCommitment::CHANGED => {
                let hash: &[u8; 32] = self.buf[self.offset..self.offset + 32].try_into().unwrap();
                self.offset += 32;
                Some(OutputResourceCommitment::Changed(hash))
            }
            OutputResourceCommitment::UNCHANGED => Some(OutputResourceCommitment::Unchanged),
            _ => panic!("invalid resource output flag: {flag}"),
        }
    }
}
