use std::{marker::PhantomData, path::Path, sync::Arc};

use rocksdb::{DB, DBIteratorWithThreadMode, Direction, IteratorMode};
use vprogs_core_smt::{Key, Node, StaleNode, Tree, WriteBatch as SmtWriteBatch};
use vprogs_storage_types::{PrefixIterator, StateSpace, Store};

use crate::{
    config::{Config, DefaultConfig},
    key_ext::KeyExt,
    stale_node_ext::StaleNodeExt,
    state_space_ext::StateSpaceExt,
    write_batch::WriteBatch,
};

pub struct RocksDbStore<C: Config = DefaultConfig> {
    db: Arc<DB>,
    write_opts: Arc<rocksdb::WriteOptions>,
    _marker: PhantomData<C>,
}

impl<C: Config> RocksDbStore<C> {
    pub fn open<P: AsRef<Path>>(path: P) -> Self {
        let mut db_opts = C::db_opts();
        db_opts.create_if_missing(true);
        db_opts.create_missing_column_families(true);

        Self {
            db: Arc::new(
                match DB::open_cf_descriptors(
                    &db_opts,
                    path,
                    <StateSpace as StateSpaceExt<C>>::all_descriptors(),
                ) {
                    Ok(db) => db,
                    Err(e) => panic!("failed to open RocksDB: {e}"),
                },
            ),
            write_opts: Arc::new(C::write_opts()),
            _marker: PhantomData,
        }
    }

    fn cf(&self, ns: &StateSpace) -> &rocksdb::ColumnFamily {
        let cf_name = <StateSpace as StateSpaceExt<C>>::cf_name;
        match self.db.cf_handle(cf_name(ns)) {
            Some(cf) => cf,
            None => panic!("missing column family '{}'", cf_name(ns)),
        }
    }
}

impl<C: Config> Store for RocksDbStore<C> {
    type WriteBatch = WriteBatch<C>;

    fn get(&self, state_space: StateSpace, key: &[u8]) -> Option<Vec<u8>> {
        match self.db.get_cf(self.cf(&state_space), key) {
            Ok(res) => res,
            Err(e) => panic!("rocksdb get failed: {e}"),
        }
    }

    fn write_batch(&self) -> WriteBatch<C> {
        WriteBatch::new(self.db.clone())
    }

    fn commit(&self, write_batch: WriteBatch<C>) {
        if let Err(err) = self.db.write_opt(write_batch.into(), &self.write_opts) {
            panic!("rocksdb write-batch commit failed: {err}");
        }
    }

    fn prefix_iter(&self, state_space: StateSpace, prefix: &[u8]) -> PrefixIterator<'_> {
        let cf = self.cf(&state_space);

        let mut read_opts = rocksdb::ReadOptions::default();
        // Ensure iteration stops when keys no longer share the prefix.
        read_opts.set_prefix_same_as_start(true);

        let mode = IteratorMode::From(prefix, Direction::Forward);
        let iter = self.db.iterator_cf_opt(cf, read_opts, mode);
        Box::new(RocksDbPrefixIter { inner: iter })
    }
}

impl<C: Config> Tree for RocksDbStore<C> {
    type Hasher = vprogs_core_smt::Blake3;

    fn get_node(&self, key: &Key, max_version: u64) -> Option<(u64, Node)> {
        let mut iter = self.prefix_iter(StateSpace::SmtNode, &key.encode_with_version(max_version));
        let (raw_key, raw_value) = iter.next()?;
        let version = Key::decode_version(&raw_key).expect("corrupted smt node key");
        let node = Node::decode(&mut raw_value.as_ref()).expect("corrupted smt node");
        Some((version, node))
    }

    fn prune(&self, wb: &mut impl SmtWriteBatch, version: u64) {
        for (raw_key, raw_value) in self.prefix_iter(StateSpace::SmtStale, &version.to_be_bytes()) {
            let node_key = StaleNode::decode_key(&raw_key).expect("corrupted stale key");
            let node_version = StaleNode::decode_value(&raw_value).expect("corrupted stale value");

            wb.delete_node(&node_key, node_version);
            wb.delete_stale_node(&StaleNode::new(version, node_key, node_version));
        }
    }

    fn rollback(&self, wb: &mut impl SmtWriteBatch, version: u64) {
        // Delete all stale markers recorded at this version. These markers reference nodes that
        // were superseded when this version was committed — removing them "un-supersedes" those
        // nodes so they become current again.
        for (raw_key, raw_value) in self.prefix_iter(StateSpace::SmtStale, &version.to_be_bytes()) {
            let node_key = StaleNode::decode_key(&raw_key).expect("corrupted stale key");
            let node_version = StaleNode::decode_value(&raw_value).expect("corrupted stale value");
            wb.delete_stale_node(&StaleNode::new(version, node_key, node_version));
        }

        // Delete all nodes written at this version. Requires a full CF scan since version is a
        // key suffix, not a prefix. Rollback is rare, so the scan cost is acceptable.
        let cf = self.cf(&StateSpace::SmtNode);
        let iter = self.db.iterator_cf(cf, IteratorMode::Start);
        for entry in iter {
            let (raw_key, _) = entry.expect("rocksdb iteration failed");
            let node_version = Key::decode_version(&raw_key).expect("corrupted smt node key");
            if node_version == version {
                let node_key = Key::decode(&mut &raw_key[..34]).expect("corrupted smt node key");
                wb.delete_node(&node_key, version);
            }
        }
    }
}

impl<C: Config> Clone for RocksDbStore<C> {
    fn clone(&self) -> Self {
        RocksDbStore {
            db: self.db.clone(),
            write_opts: self.write_opts.clone(),
            _marker: PhantomData,
        }
    }
}

/// Wrapper around RocksDB's prefix iterator that unwraps Results into panics.
struct RocksDbPrefixIter<'a> {
    inner: DBIteratorWithThreadMode<'a, DB>,
}

impl Iterator for RocksDbPrefixIter<'_> {
    type Item = (Vec<u8>, Vec<u8>);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|res| match res {
            Ok((k, v)) => (k.to_vec(), v.to_vec()),
            Err(e) => panic!("rocksdb prefix iteration failed: {e}"),
        })
    }
}
