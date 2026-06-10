use kaspa_hashes::Hash;

use crate::{
    transaction_processor::{Resource, Transaction},
    withdrawal::ExitSink,
};

/// Developer-provided transaction execution logic.
///
/// Receives the full parsed transaction, its merge_idx, context hash, mutable access to the
/// transaction's resources, and an [`ExitSink`] to emit L2→L1 withdrawals.
/// Returns `Ok(())` on success or an error that gets committed to the journal.
pub trait TransactionHandler:
    for<'a> FnOnce(
    &Transaction<'a>,
    u32,
    &'a Hash,
    &mut [Resource<'a>],
    &mut ExitSink,
) -> crate::Result<()>
{
}

impl<F> TransactionHandler for F where
    F: for<'a> FnOnce(
        &Transaction<'a>,
        u32,
        &'a Hash,
        &mut [Resource<'a>],
        &mut ExitSink,
    ) -> crate::Result<()>
{
}
