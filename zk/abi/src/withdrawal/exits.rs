use zerocopy::{FromBytes, Immutable, KnownLayout, Unaligned};

use crate::{
    Result,
    withdrawal::{ExitsIter, StandardSpk},
};

/// Zero-copy view over a serialized run of `(destination, amount)` exit entries.
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
