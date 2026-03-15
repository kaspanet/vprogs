use vprogs_core_crypto::smt::TreeWriteBatch;

use crate::StateSpace;

pub trait WriteBatch: TreeWriteBatch {
    fn put(&mut self, ns: StateSpace, key: &[u8], value: &[u8]);
    fn delete(&mut self, ns: StateSpace, key: &[u8]);
}
