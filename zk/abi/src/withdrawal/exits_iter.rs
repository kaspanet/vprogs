use vprogs_core_codec::Reader;

use crate::{Result, withdrawal::StandardSpk};

/// Forward iterator over an [`Exits`] view's entries. Streams to EOF; each `next` advances the
/// internal cursor past one `(destination, amount)` pair.
///
/// [`Exits`]: crate::withdrawal::Exits
pub struct ExitsIter<'a> {
    /// Remaining unparsed exit bytes; constructed by
    /// [`Exits::iter`](crate::withdrawal::Exits::iter).
    pub(crate) buf: &'a [u8],
}

impl<'a> Iterator for ExitsIter<'a> {
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
