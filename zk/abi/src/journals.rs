use vprogs_core_codec::Reader;

use crate::{Error, Result};

/// Iterator over length-prefixed journal slices packed back-to-back in a buffer.
#[derive(Clone, Copy)]
pub struct Journals<'a> {
    /// Remaining unconsumed bytes of the journal entries.
    buf: &'a [u8],
}

impl<'a> Journals<'a> {
    /// Creates a new iterator over length-prefixed journal entries in `buf`.
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    /// Returns true if no unconsumed journal entries remain.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

impl<'a> Iterator for Journals<'a> {
    type Item = Result<&'a [u8]>;

    fn next(&mut self) -> Option<Self::Item> {
        // An empty buffer ends iteration; otherwise decode the next entry and fuse on error.
        (!self.buf.is_empty()).then(|| {
            self.buf.blob("journal").map_err(|e| {
                self.buf = &[];
                Error::from(e)
            })
        })
    }
}
