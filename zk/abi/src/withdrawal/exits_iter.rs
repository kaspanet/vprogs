use vprogs_core_codec::Reader;

use crate::{Result, withdrawal::StandardSpk};

/// Iterator over an [`Exits`](crate::withdrawal::Exits) view's `(destination, amount)` entries.
pub struct ExitsIter<'a> {
    /// Remaining unparsed exit bytes.
    pub(crate) buf: &'a [u8],
}

impl<'a> Iterator for ExitsIter<'a> {
    type Item = Result<(StandardSpk<'a>, u64)>;

    fn next(&mut self) -> Option<Self::Item> {
        (!self.buf.is_empty())
            .then(|| Ok((StandardSpk::decode(&mut self.buf)?, self.buf.le_u64("exit_amount")?)))
    }
}
