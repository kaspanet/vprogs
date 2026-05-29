use zerocopy::{Immutable, IntoBytes, KnownLayout, TryFromBytes, Unaligned, little_endian::U32};

/// SMT membership claim with the leaf index that backs it.
#[repr(u8)]
#[derive(TryFromBytes, IntoBytes, Immutable, KnownLayout, Unaligned)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Membership {
    /// Present element with its leaf index.
    Present(U32) = 0,
    /// Absent element with the witness leaf's index.
    Absent(U32) = 1,
}
