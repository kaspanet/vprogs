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

    /// Returns true if no unconsumed journal entries remain.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
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

        match self.decode_entry() {
            Ok(entry) => Some(Ok(entry)),
            Err(e) => {
                // Fuse on error: a bad length prefix corrupts every following entry.
                self.buf = &[];
                Some(Err(e))
            }
        }
    }
}
