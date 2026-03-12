use crate::{Parser, Result};

/// Iterator over per-transaction journal slices in a batch witness.
pub struct JournalIter<'a> {
    /// Remaining unconsumed bytes of the journal entries.
    buf: &'a [u8],
    /// Number of entries not yet yielded.
    remaining: u32,
}

impl<'a> JournalIter<'a> {
    /// Creates a new iterator over `remaining` length-prefixed journal entries in `buf`.
    pub fn new(buf: &'a [u8], remaining: u32) -> Self {
        Self { buf, remaining }
    }
}

impl<'a> Iterator for JournalIter<'a> {
    type Item = Result<&'a [u8]>;

    fn next(&mut self) -> Option<Self::Item> {
        // Check if all entries have been consumed.
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;

        // Read length prefix.
        let length = match self.buf[..4].parse_u32("journal_length") {
            Ok(len) => len as usize,
            Err(e) => return Some(Err(e)),
        };

        // Advance past consumed bytes.
        let journal = &self.buf[4..4 + length];
        self.buf = &self.buf[4 + length..];

        Some(Ok(journal))
    }
}
