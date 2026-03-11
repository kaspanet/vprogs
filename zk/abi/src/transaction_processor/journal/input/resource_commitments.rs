use crate::transaction_processor::InputResourceCommitment;

/// Zero-copy iterator over resource input commitment entries.
pub struct InputResourceCommitments<'a> {
    buf: &'a [u8],
    remaining: u32,
}

impl<'a> InputResourceCommitments<'a> {
    /// Creates a new iterator over `remaining` fixed-size entries in `buf`.
    pub fn new(buf: &'a [u8], remaining: u32) -> Self {
        Self { buf, remaining }
    }
}

impl<'a> Iterator for InputResourceCommitments<'a> {
    type Item = InputResourceCommitment<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;

        let commitment = InputResourceCommitment::decode(self.buf);
        self.buf = &self.buf[InputResourceCommitment::SIZE..];

        Some(commitment)
    }
}

impl ExactSizeIterator for InputResourceCommitments<'_> {
    fn len(&self) -> usize {
        self.remaining as usize
    }
}
