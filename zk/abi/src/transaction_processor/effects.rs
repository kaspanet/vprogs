use crate::transaction_processor::{ExitSink, Resource};

/// Container for the side effects produced by a successful transaction handler invocation.
pub struct Effects<'a> {
    /// Buffered L2 -> L1 exit entries emitted during execution.
    pub exits: &'a ExitSink,
    /// Resource view after the handler ran (initial state plus in-place mutations).
    pub resources: &'a [Resource<'a>],
}
