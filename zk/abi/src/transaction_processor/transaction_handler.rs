use crate::transaction_processor::Resource;

/// Developer-provided transaction execution logic.
///
/// Receives the raw transaction bytes, batch position, context hash, and mutable access to the
/// transaction's resources. Returns `Ok(())` on success or an error that gets committed to the
/// journal.
pub trait TransactionHandler:
    for<'a> FnOnce(&'a [u8], u32, &'a [u8; 32], &mut [Resource<'a>]) -> crate::Result<()>
{
}

impl<F> TransactionHandler for F where
    F: for<'a> FnOnce(&'a [u8], u32, &'a [u8; 32], &mut [Resource<'a>]) -> crate::Result<()>
{
}
