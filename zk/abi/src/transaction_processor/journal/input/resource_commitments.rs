use crate::transaction_processor::InputResourceCommitment;

/// Zero-copy iterator over resource input commitment entries.
pub struct InputResourceCommitments<'a> {
    pub(crate) buf: &'a [u8],
    pub(crate) offset: usize,
    pub(crate) remaining: u32,
}

impl<'a> Iterator for InputResourceCommitments<'a> {
    type Item = InputResourceCommitment<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;

        let o = self.offset;
        let commitment = InputResourceCommitment::decode(&self.buf[o..]);
        self.offset = o + InputResourceCommitment::SIZE;

        Some(commitment)
    }
}

impl ExactSizeIterator for InputResourceCommitments<'_> {
    fn len(&self) -> usize {
        self.remaining as usize
    }
}
