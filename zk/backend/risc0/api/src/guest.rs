use vprogs_zk_abi::transaction_processor::{Abi, TransactionHandler};

use crate::{Host, Journal};

/// RISC-0 guest entry point - wraps host/journal wiring for transaction processing.
pub struct Guest;

impl Guest {
    /// Reads a transaction from the host, executes the developer-provided handler, and commits
    /// the result to the journal.
    pub fn process_transaction(f: impl TransactionHandler) {
        Abi::process_transaction(&mut Host, &mut Journal, f);
    }
}
