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
        let resource_index = u32::from_le_bytes(self.buf[o..o + 4].try_into().unwrap());
        let resource_id: &[u8; 32] = self.buf[o + 4..o + 36].try_into().unwrap();
        let input_hash: &[u8; 32] = self.buf[o + 36..o + 68].try_into().unwrap();
        self.offset = o + 68;

        Some(ResourceInputCommitment { resource_index, resource_id, input_hash })
    }
}

impl ExactSizeIterator for ResourceInputCommitments<'_> {
    fn len(&self) -> usize {
        self.remaining as usize
    }
}
