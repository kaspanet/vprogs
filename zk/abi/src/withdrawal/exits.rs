use zerocopy::{FromBytes, Immutable, KnownLayout, Unaligned};

use crate::{
    Result,
    withdrawal::{ExitsIter, StandardSpk},
};

/// Zero-copy view over a serialized exit section: the exits emitted by a
/// [`OutputCommitment::Success`](crate::transaction_processor::OutputCommitment::Success), carried
/// forward as the trailing-DST field of a
/// [`BatchTransition`](crate::batch_processor::BatchTransition).
///
/// Not a hash commitment -- it is how exits are communicated between guests; the aggregator folds
/// them into the bundle's permission commitment.
///
/// `#[repr(transparent)]` over `[u8]` so it parses zero-copy with [`FromBytes::ref_from_bytes`] and
/// can sit as the trailing slice of an outer DST. Iterate by reference -- `for exit in &exits` --
/// or explicitly via [`iter`](Self::iter).
#[repr(C)]
#[derive(FromBytes, Immutable, KnownLayout, Unaligned)]
pub struct Exits {
    buf: [u8],
}

impl Exits {
    /// Returns `true` if no entries remain.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Iterates the decoded `(destination, amount)` entries in journal order.
    pub fn iter(&self) -> ExitsIter<'_> {
        ExitsIter { buf: &self.buf }
    }
}

impl<'a> IntoIterator for &'a Exits {
    type Item = Result<(StandardSpk<'a>, u64)>;
    type IntoIter = ExitsIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}
