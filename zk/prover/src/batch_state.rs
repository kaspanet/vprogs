use std::collections::HashMap;

use vprogs_zk_vm::ProofRequest;

use crate::account_data::AccountData;

/// Per-batch proving state.
pub(crate) struct BatchState<R> {
    /// Expected number of transactions in this batch.
    pub(crate) expected_tx_count: u32,
    /// Collected receipts, keyed by tx_index.
    pub(crate) receipts: Vec<(u32, R, ProofRequest)>,
    /// Pre-batch account data, keyed by account_index. Only the first occurrence
    /// (in causal order) is recorded — that's the pre-batch state.
    pub(crate) pre_batch_accounts: HashMap<u32, AccountData>,
}
