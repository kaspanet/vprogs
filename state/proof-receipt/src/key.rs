use zerocopy::{
    FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned,
    byteorder::big_endian::{U32, U64},
};

/// The column family's prefix-extractor unit and the granularity [`invalidate_checkpoint`]
/// prunes by: the checkpoint index alone. Every receipt key begins with it, so one prefix scan
/// reaches every program's receipts for one checkpoint, whatever their image id or block hash.
///
/// [`invalidate_checkpoint`]: crate::invalidate_checkpoint
#[derive(Clone, Copy, IntoBytes, FromBytes, KnownLayout, Immutable, Unaligned)]
#[repr(transparent)]
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

/// Key for a per-batch receipt: `prefix || block_hash(32) || image_id`.
///
/// `block_hash` is the chain block this receipt was proven against. It sits right after the
/// prefix so that `checkpoint_index || block_hash` reads as one fork-aware chain coordinate:
/// the same checkpoint index on two competing chains yields two distinct keys, yet both still
/// share the prefix, so [`invalidate_checkpoint`] drops them together. Lookups are by exact key
/// -- the scheduler only requests proofs for blocks on its single canonical chain, and each
/// chain-block metadata carries the hash.
///
/// [`invalidate_checkpoint`]: crate::invalidate_checkpoint
#[derive(Clone, Copy, IntoBytes, FromBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
pub struct BatchKey {
    pub prefix: Prefix,
    pub block_hash: [u8; 32],
    pub image_id: [u8; 32],
}

/// Key for a per-transaction receipt: `prefix || block_hash(32) || image_id || merge_idx(4 BE)`.
#[derive(Clone, Copy, IntoBytes, FromBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
pub struct TxKey {
    pub prefix: Prefix,
    pub block_hash: [u8; 32],
    pub image_id: [u8; 32],
    pub merge_idx: U32,
}

/// Key for an aggregated-bundle receipt:
/// `prefix || block_hash(32) || image_id || seq_commit(32)`.
///
/// The prefix's `checkpoint_index` is the bundle's first checkpoint index and `seq_commit`
/// the claimed tip commitment, so together they pin the bundle's `from -> to` range: two
/// bundles ending at the same tip but starting from different settled points never share a
/// key. `block_hash` is the chain block at that first index, keeping bundles that began on
/// competing forks distinct.
#[derive(Clone, Copy, IntoBytes, FromBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
pub struct AggregatorKey {
    pub prefix: Prefix,
    pub block_hash: [u8; 32],
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
