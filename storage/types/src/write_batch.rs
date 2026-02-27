use crate::StateSpace;

pub trait WriteBatch {
    fn put(&mut self, ns: StateSpace, key: &[u8], value: &[u8]);
    fn delete(&mut self, ns: StateSpace, key: &[u8]);
}
