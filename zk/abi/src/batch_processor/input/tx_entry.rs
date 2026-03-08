/// A single transaction entry in the batch witness.
pub struct TxEntry<'a> {
    pub journal: &'a [u8],
    pub wire_bytes: &'a [u8],
    pub exec_result: &'a [u8],
}

/// Iterator over transaction entries in a batch witness.
pub struct TxEntryIter<'a> {
    pub(super) buf: &'a [u8],
    pub(super) offset: usize,
    pub(super) remaining: u32,
}

impl<'a> Iterator for TxEntryIter<'a> {
    type Item = TxEntry<'a>;

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

        let wire_bytes_len = u32::from_le_bytes(
            self.buf[self.offset..self.offset + 4].try_into().expect("truncated wire_bytes_len"),
        ) as usize;
        self.offset += 4;
        let wire_bytes = &self.buf[self.offset..self.offset + wire_bytes_len];
        self.offset += wire_bytes_len;

        let exec_result_len = u32::from_le_bytes(
            self.buf[self.offset..self.offset + 4].try_into().expect("truncated exec_result_len"),
        ) as usize;
        self.offset += 4;
        let exec_result = &self.buf[self.offset..self.offset + exec_result_len];
        self.offset += exec_result_len;

        Some(TxEntry { journal, wire_bytes, exec_result })
    }
}
