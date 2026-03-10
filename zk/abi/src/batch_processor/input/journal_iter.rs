/// Iterator over per-transaction journal slices in a batch witness.
pub struct JournalIter<'a> {
    pub(super) buf: &'a [u8],
    pub(super) offset: usize,
    pub(super) remaining: u32,
}

impl<'a> Iterator for JournalIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;

        let journal_len = u32::from_le_bytes(
            self.buf[self.offset..self.offset + 4].try_into().expect("truncated journal_len"),
        ) as usize;
        self.offset += 4;
        let journal = &self.buf[self.offset..self.offset + journal_len];
        self.offset += journal_len;

        Some(journal)
    }
}
