use std::{marker::PhantomData, sync::Arc};

use rocksdb::DB;
use vprogs_core_smt::{Key, Node, StaleNode, WriteBatch as SmtWriteBatch};
use vprogs_storage_types::{StateSpace, WriteBatch as _};

use crate::{
    config::{Config, DefaultConfig},
    key_ext::KeyExt,
    stale_node_ext::StaleNodeExt,
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

impl<C: Config> SmtWriteBatch for WriteBatch<C> {
    fn put_node(&mut self, node_key: &Key, version: u64, data: &Node) {
        self.put(StateSpace::SmtNode, &node_key.encode_with_version(version), &data.encode());
    }

    fn put_stale_node(&mut self, stale: &StaleNode) {
        self.put(StateSpace::SmtStale, &stale.encode_key(), &stale.encode_value());
    }

    fn delete_node(&mut self, node_key: &Key, version: u64) {
        self.delete(StateSpace::SmtNode, &node_key.encode_with_version(version));
    }

    fn delete_stale_node(&mut self, stale: &StaleNode) {
        self.delete(StateSpace::SmtStale, &stale.encode_key());
    }
}

impl<C: Config> From<WriteBatch<C>> for rocksdb::WriteBatch {
    fn from(value: WriteBatch<C>) -> Self {
        value.inner
    }
}
