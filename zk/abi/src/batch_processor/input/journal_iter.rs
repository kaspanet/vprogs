/// Iterator over per-transaction journal slices in a batch witness.
pub struct JournalIter<'a> {
    buf: &'a [u8],
    remaining: u32,
}

impl<'a> JournalIter<'a> {
    /// Creates a new iterator over `remaining` length-prefixed journal entries in `buf`.
    pub fn new(buf: &'a [u8], remaining: u32) -> Self {
        Self { buf, remaining }
    }
}

impl<'a> Iterator for JournalIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;

        // Read length prefix and advance past it.
        let len =
            u32::from_le_bytes(self.buf[..4].try_into().expect("truncated journal_len")) as usize;
        let journal = &self.buf[4..4 + len];
        self.buf = &self.buf[4 + len..];

        Some(journal)
    }
}
