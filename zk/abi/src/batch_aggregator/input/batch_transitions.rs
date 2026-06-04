use vprogs_core_codec::{Reader, Result};

/// Iterator over per-batch journal slices in an aggregator input.
///
/// Each entry is a length-prefixed [`BatchTransition`] journal (fixed header + length-prefixed
/// trailing exits blob), the same bytes the per-batch guest's `env::commit` produced. The
/// aggregator iterates them in scheduling order, runs `env::verify` against the configured
/// `batch_image_id`, decodes the header to apply chain conditions, and streams the trailing
/// exits into its permission tree.
///
/// [`BatchTransition`]: crate::batch_processor::BatchTransition
#[derive(Clone, Copy)]
pub struct BatchTransitions<'a> {
    /// Remaining unconsumed bytes of the journal entries.
    buf: &'a [u8],
}

impl<'a> BatchTransitions<'a> {
    /// Creates a new iterator over length-prefixed journal entries in `buf`.
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    /// Returns `true` if no unconsumed journal entries remain.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Decodes a single length-prefixed journal entry from the buffer.
    fn decode_entry(&mut self) -> Result<&'a [u8]> {
        self.buf.blob("journal")
    }
}

impl<'a> Iterator for BatchTransitions<'a> {
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
