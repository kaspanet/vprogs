use super::resource_input_commitment::ResourceInputCommitment;

/// Zero-copy iterator over resource input commitment entries.
pub struct ResourceInputCommitments<'a> {
    pub(crate) buf: &'a [u8],
    pub(crate) offset: usize,
    pub(crate) remaining: u32,
}

impl<'a> Iterator for ResourceInputCommitments<'a> {
    type Item = ResourceInputCommitment<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;

        let o = self.offset;
        let commitment = ResourceInputCommitment::decode(&self.buf[o..]);
        self.offset = o + super::resource_input_commitment::SIZE;

        Some(commitment)
    }
}

impl ExactSizeIterator for ResourceInputCommitments<'_> {
    fn len(&self) -> usize {
        self.remaining as usize
    }
}
