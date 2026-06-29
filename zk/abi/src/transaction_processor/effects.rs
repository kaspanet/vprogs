use crate::{transaction_processor::Resource, withdrawal::ExitSink};

/// Container for the side effects produced by a successful transaction handler invocation.
pub struct Effects<'a> {
    /// Buffered L2 -> L1 exit entries emitted during execution.
    pub exits: &'a ExitSink,
    /// Deposit-address commitment for this tx, or `[0u8; 32]` when it credited no L1 deposit.
    pub deposit_spk_hash: &'a [u8; 32],
    /// Resource view after the handler ran (initial state plus in-place mutations).
    pub resources: &'a [Resource<'a>],
}
