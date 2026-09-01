use crate::StateSpace;

pub trait WriteBatch: vprogs_core_smt::WriteBatch {
    fn put(&mut self, ns: StateSpace, key: &[u8], value: &[u8]);
    fn delete(&mut self, ns: StateSpace, key: &[u8]);
}
