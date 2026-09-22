use crate::{
    transaction_processor::{MergesetContext, Resource, Transaction},
    withdrawal::{DepositSink, ExitSink},
};

/// Developer-provided transaction execution logic.
///
/// Receives the full parsed transaction, its merge_idx, the chain block's mergeset context,
/// mutable access to the transaction's resources, an [`ExitSink`] to emit L2→L1 withdrawals, and
/// a [`DepositSink`] to record the deposit-address commitment when the tx credits an L1 deposit.
/// Returns `Ok(())` on success or an error that gets committed to the journal.
pub trait TransactionHandler:
    for<'a> FnOnce(
    &Transaction<'a>,
    u32,
    &'a MergesetContext,
    &mut [Resource<'a>],
    &mut ExitSink,
    &mut DepositSink,
) -> crate::Result<()>
{
}

impl<F> TransactionHandler for F where
    F: for<'a> FnOnce(
        &Transaction<'a>,
        u32,
        &'a MergesetContext,
        &mut [Resource<'a>],
        &mut ExitSink,
        &mut DepositSink,
    ) -> crate::Result<()>
{
}
