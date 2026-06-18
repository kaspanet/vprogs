/// Running totals the driver maintains, readable after a run for end-of-test assertions.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct DriverStats {
    /// Selected-chain blocks scheduled (net of rollbacks).
    pub blocks_processed: u64,
    /// Lane-activity transactions executed on the current selected chain.
    pub activity_executed: u64,
    /// Reorgs observed (non-empty removed sets).
    pub reorgs: u64,
    /// Deepest reorg (blocks rolled back in one event).
    pub max_reorg_depth: u64,
    /// Covenant settlement transactions issued (each emission, including a re-issue of an orphaned
    /// settlement, counts).
    pub settlements_issued: u64,
    /// Covenant settlements that landed and chained successfully.
    pub settlements_accepted: u64,
    /// Covenant txs re-issued after their block was orphaned by a reorg (0 on a clean chain).
    pub reissues: u64,
}
