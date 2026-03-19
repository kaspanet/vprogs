use vprogs_core_codec::Reader;

use crate::Result;

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

    /// Decodes a single length-prefixed journal entry from the buffer.
    fn decode_entry(&mut self) -> Result<&'a [u8]> {
        let length = self.buf.le_u32("journal_length")? as usize;
        Ok(self.buf.bytes(length, "journal")?)
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

        Some(self.decode_entry())
    }
}
