//! Persistent cache for proven receipts, keyed by the semantic coordinates a caller knows
//! before proving so it can skip work it has already done.
//!
//! The crate is framework-agnostic: an `image_id` is an opaque 32-byte program identifier and
//! a receipt is an opaque `R: Borsh{Serialize,Deserialize}` blob -- nothing here names a proof
//! system. One generic store serves every receipt kind; the kind is picked by the
//! [`ReceiptKey`] type ([`TxKey`], [`BatchKey`], [`AggregatorKey`]), and every key starts
//! with the shared [`Prefix`] the [`StateSpace::ProofReceipt`] column family's extractor and
//! pruning rely on.
mod key;
mod prune;

pub use key::{AggregatorKey, BatchKey, HasPrefix, Prefix, ReceiptKey, TxKey};
pub use prune::invalidate_checkpoint;
use vprogs_storage_types::{StateSpace, Store, WriteBatch};

/// Returns the cached receipt at `key`, or `None` on a cache miss.
pub fn get<R, S>(store: &S, key: &impl ReceiptKey) -> Option<R>
where
    R: borsh::BorshDeserialize,
    S: Store,
{
    store
        .get(StateSpace::ProofReceipt, key.as_bytes())
        .map(|bytes| borsh::from_slice(&bytes).expect("corrupted proof-receipt store"))
}

/// Writes the receipt at `key` into `wb`.
pub fn put<R, W>(wb: &mut W, key: &impl ReceiptKey, receipt: &R)
where
    R: borsh::BorshSerialize,
    W: WriteBatch,
{
    wb.put(
        StateSpace::ProofReceipt,
        key.as_bytes(),
        &borsh::to_vec(receipt).expect("failed to serialize receipt"),
    );
}

/// Removes the cached receipt at `key`.
pub fn delete<W: WriteBatch>(wb: &mut W, key: &impl ReceiptKey) {
    wb.delete(StateSpace::ProofReceipt, key.as_bytes());
}
