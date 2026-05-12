use vprogs_core_codec::{Error, Reader, Result};

use crate::batch_processor::Batch;

/// Iterator over the batches in `Inputs`.
#[derive(Clone, Copy)]
pub struct Batches<'a> {
    /// Number of batches left to decode.
    remaining: u32,
    /// Remaining unconsumed bytes of the batch entries.
    buf: &'a [u8],
}

impl<'a> Batches<'a> {
    /// Decodes the count prefix and returns an iterator over the trailing batch entries.
    pub fn decode(mut buf: &'a [u8]) -> Result<Self> {
        Ok(Self { remaining: buf.le_u32("batches_count")?, buf })
    }

    /// Returns true if no batches remain to decode.
    pub fn is_empty(&self) -> bool {
        self.remaining == 0
    }

    /// Decodes the first batch without advancing the stored iterator.
    pub fn first(mut self) -> Result<Batch<'a>> {
        self.next().unwrap_or(Err(Error::Decode("empty bundle")))
    }
}

impl<'a> Iterator for Batches<'a> {
    type Item = Result<Batch<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;

        match Batch::decode(&mut self.buf) {
            Ok(b) => Some(Ok(b)),
            Err(e) => {
                // Fuse on error: a bad batch corrupts every following entry.
                self.remaining = 0;
                Some(Err(e))
            }
        }
    }
}
