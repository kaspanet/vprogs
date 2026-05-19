use vprogs_core_codec::Reader;

use crate::{Result, transaction_processor::StandardSpk};

/// Zero-copy iterator over the per-tx exit section of an
/// [`OutputCommitment::Success`](crate::transaction_processor::OutputCommitment::Success).
///
/// Streams to EOF; entries are read until the underlying buffer is exhausted.
#[derive(Clone, Copy)]
pub struct ExitCommitment<'a> {
    buf: &'a [u8],
}

impl<'a> ExitCommitment<'a> {
    /// Creates a new iterator over the remaining bytes of the exit section.
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    /// Returns the raw remaining bytes (debug / round-trip aid).
    pub fn as_bytes(&self) -> &'a [u8] {
        self.buf
    }

    /// Returns `true` if no entries remain.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

impl<'a> Iterator for ExitCommitment<'a> {
    type Item = Result<(StandardSpk<'a>, u64)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.buf.is_empty() {
            return None;
        }
        Some((|| {
            let dest = StandardSpk::decode(&mut self.buf)?;
            let amount = self.buf.le_u64("exit_amount")?;
            Ok((dest, amount))
        })())
    }
}
