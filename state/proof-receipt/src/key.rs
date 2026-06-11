use zerocopy::{Immutable, IntoBytes, Unaligned, byteorder::big_endian::{U32, U64}, FromBytes, KnownLayout};

/// The column family's prefix-extractor unit and the granularity [`invalidate_checkpoint`]
/// prunes by: the checkpoint index alone. Every receipt key begins with it, so one prefix scan
/// reaches every program's receipts for one checkpoint, whatever their image id.
///
/// [`invalidate_checkpoint`]: crate::invalidate_checkpoint
#[derive(Clone, Copy, IntoBytes, FromBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
pub struct Prefix {
    pub checkpoint_index: U64,
}

/// Proof that a key starts with the shared [`Prefix`]: concrete keys embed it as their first
/// `repr(C)` field and lend it out here, which also guarantees
/// `size_of::<Self>() >= size_of::<Prefix>()`.
pub trait HasPrefix {
    fn prefix(&self) -> &Prefix;
}

/// A proof-receipt cache key: `as_bytes()` yields the key bytes, which start with the shared
/// [`Prefix`]. Blanket-implemented for every [`HasPrefix`] key -- the blanket is the only
/// impl, so a key cannot opt out of the prefix.
pub trait ReceiptKey: IntoBytes + Immutable + HasPrefix {
    /// The checkpoint index the receipt belongs to ([`AggregatorKey`]'s bundle-start index).
    fn checkpoint_index(&self) -> U64 {
        self.prefix().checkpoint_index
    }
}

impl<K: IntoBytes + Immutable + HasPrefix> ReceiptKey for K {}

/// Key for a per-batch receipt: `prefix || image_id`.
#[derive(Clone, Copy, IntoBytes, FromBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
pub struct BatchKey {
    pub prefix: Prefix,
    pub image_id: [u8; 32],
}

/// Key for a per-transaction receipt: `prefix || image_id || merge_idx(4 BE)`.
#[derive(Clone, Copy, IntoBytes, FromBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
pub struct TxKey {
    pub prefix: Prefix,
    pub image_id: [u8; 32],
    pub merge_idx: U32,
}

/// Key for an aggregated-bundle receipt: `prefix || image_id || seq_commit(32)`.
///
/// The prefix's `checkpoint_index` is the bundle's first checkpoint index and `seq_commit`
/// the claimed tip commitment, so together they pin the bundle's `from -> to` range: two
/// bundles ending at the same tip but starting from different settled points never share a
/// key.
#[derive(Clone, Copy, IntoBytes, FromBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
pub struct AggregatorKey {
    pub prefix: Prefix,
    pub image_id: [u8; 32],
    pub seq_commit: [u8; 32],
}

impl HasPrefix for BatchKey {
    fn prefix(&self) -> &Prefix {
        &self.prefix
    }
}

impl HasPrefix for TxKey {
    fn prefix(&self) -> &Prefix {
        &self.prefix
    }
}

impl HasPrefix for AggregatorKey {
    fn prefix(&self) -> &Prefix {
        &self.prefix
    }
}
