use crate::transaction_processor::{Resource, Transaction};

/// Developer-provided transaction execution logic.
///
/// Receives the full parsed transaction, its merge_idx, context hash, and mutable access to the
/// transaction's resources. Returns `Ok(())` on success or an error that gets committed to the
/// journal.
pub trait TransactionHandler:
    for<'a> FnOnce(&Transaction<'a>, u32, &'a [u8; 32], &mut [Resource<'a>]) -> crate::Result<()>
{
}

impl<F> TransactionHandler for F where
    F: for<'a> FnOnce(
        &Transaction<'a>,
        u32,
        &'a [u8; 32],
        &mut [Resource<'a>],
    ) -> crate::Result<()>
{
}
