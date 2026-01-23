use crate::Access;

/// A test transaction that writes its ID to each accessed resource.
pub struct Tx(pub usize, pub Vec<Access>);

impl vprogs_core_types::Transaction<usize, Access> for Tx {
    fn accessed_resources(&self) -> &[Access] {
        &self.1
    }
}
