use super::resource_output_commitment::ResourceOutputCommitment;

/// Flag byte: resource data was modified (32-byte hash follows).
pub(crate) const CHANGED: u8 = 0x01;
/// Flag byte: resource data was not modified (no hash follows).
pub(crate) const UNCHANGED: u8 = 0x00;

/// Zero-copy iterator over per-resource output hash commitments.
pub struct ResourceOutputCommitments<'a> {
    pub(crate) buf: &'a [u8],
    pub(crate) offset: usize,
}

impl<'a> Iterator for ResourceOutputCommitments<'a> {
    type Item = ResourceOutputCommitment<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.buf.len() {
            return None;
        }

        let flag = self.buf[self.offset];
        self.offset += 1;

        match flag {
            CHANGED => {
                let hash: &[u8; 32] = self.buf[self.offset..self.offset + 32].try_into().unwrap();
                self.offset += 32;
                Some(Some(hash))
            }
            UNCHANGED => Some(None),
            _ => panic!("invalid resource output flag: {flag}"),
        }
    }
}
