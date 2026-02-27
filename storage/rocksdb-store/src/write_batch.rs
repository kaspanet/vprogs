use std::{marker::PhantomData, sync::Arc};

use rocksdb::DB;
use vprogs_state_space::StateSpace;

use crate::{
    config::{Config, DefaultConfig},
    state_space_ext::StateSpaceExt,
};

pub struct WriteBatch<C: Config = DefaultConfig> {
    db: Arc<DB>,
    inner: rocksdb::WriteBatch,
    _marker: PhantomData<C>,
}

impl<C: Config> WriteBatch<C> {
    pub(crate) fn new(db: Arc<DB>) -> Self {
        Self { db, inner: rocksdb::WriteBatch::default(), _marker: PhantomData }
    }
}

impl<C: Config> vprogs_storage_types::WriteBatch for WriteBatch<C> {
    fn put(&mut self, ns: StateSpace, key: &[u8], value: &[u8]) {
        let cf_handle = <StateSpace as StateSpaceExt<C>>::cf_name(&ns);
        let Some(cf) = self.db.cf_handle(cf_handle) else {
            panic!("missing column family '{}'", cf_handle)
        };
        self.inner.put_cf(cf, key, value)
    }

    fn delete(&mut self, ns: StateSpace, key: &[u8]) {
        let cf_handle = <StateSpace as StateSpaceExt<C>>::cf_name(&ns);
        let Some(cf) = self.db.cf_handle(cf_handle) else {
            panic!("missing column family '{}'", cf_handle)
        };
        self.inner.delete_cf(cf, key)
    }
}

impl<C: Config> From<WriteBatch<C>> for rocksdb::WriteBatch {
    fn from(value: WriteBatch<C>) -> Self {
        value.inner
    }
}
