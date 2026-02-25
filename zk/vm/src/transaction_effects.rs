use std::fmt;

/// Effects produced by executing a ZK transaction.
pub struct TransactionEffects {
    pub ops: Vec<vprogs_zk_types::StateOp>,
}

impl fmt::Display for TransactionEffects {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TransactionEffects({} ops)", self.ops.len())
    }
}
