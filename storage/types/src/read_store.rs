use crate::{StateSpace, Store};

pub trait ReadStore {
    fn get(&self, ns: StateSpace, key: &[u8]) -> Option<Vec<u8>>;
}

impl<T: Store> ReadStore for T {
    fn get(&self, ns: StateSpace, key: &[u8]) -> Option<Vec<u8>> {
        Store::get(self, ns, key)
    }
}
