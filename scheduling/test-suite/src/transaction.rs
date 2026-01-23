use crate::Access;

/// A test transaction that writes its ID to each accessed resource.
pub struct Transaction(pub usize, pub Vec<Access>);

impl vprogs_core_types::Transaction<usize, Access> for Transaction {
    fn accessed_resources(&self) -> &[Access] {
        &self.1
    }
}
