use std::{marker::PhantomData, sync::Arc};

use rocksdb::DB;
use vprogs_core_crypto::smt::{Node, StaleNode, TreeWriteBatch};
use vprogs_storage_types::StateSpace;

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

impl<C: Config> TreeWriteBatch for WriteBatch<C> {
    fn put_node(&mut self, node: &Node) {
        let key = node.key.encode_cf_key(node.version);
        <Self as vprogs_storage_types::WriteBatch>::put(
            self,
            StateSpace::SmtNode,
            &key,
            &node.data.to_bytes(),
        );
    }

    fn put_stale_node(&mut self, stale: &StaleNode) {
        let key = stale.encode_cf_key();
        <Self as vprogs_storage_types::WriteBatch>::put(
            self,
            StateSpace::SmtStale,
            &key,
            &stale.node_version.to_be_bytes(),
        );
    }
}

impl<C: Config> From<WriteBatch<C>> for rocksdb::WriteBatch {
    fn from(value: WriteBatch<C>) -> Self {
        value.inner
    }
}
