//! Concrete type bindings for the node binary.
//! Change these to swap implementations at compile time.

use vprogs_storage_rocksdb_store::{DefaultConfig, RocksDbStore};

/// Transaction processor - the production VM backed by the transaction runtime.
pub type Vm = vprogs_node_vm::VM;

/// Persistence layer - RocksDB with the default column family configuration.
pub type Store = RocksDbStore<DefaultConfig>;
