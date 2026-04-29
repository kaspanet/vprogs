use vprogs_core_codec::Reader;

use crate::Result;

/// Iterator over per-transaction journal slices in a batch witness.
#[derive(Clone, Copy)]
pub struct TransactionJournals<'a> {
    /// Remaining unconsumed bytes of the journal entries.
    buf: &'a [u8],
}

impl<'a> TransactionJournals<'a> {
    /// Creates a new iterator over length-prefixed journal entries in `buf`.
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    /// Decodes a single length-prefixed journal entry from the buffer.
    fn decode_entry(&mut self) -> Result<&'a [u8]> {
        let length = self.buf.le_u32("journal_length")? as usize;
        Ok(self.buf.bytes(length, "journal")?)
    }
}

impl<'a> Iterator for TransactionJournals<'a> {
    type Item = Result<&'a [u8]>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.buf.is_empty() {
            return None;
        }

        Some(self.decode_entry())
    }
}
