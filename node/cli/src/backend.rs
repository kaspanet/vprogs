//! Concrete type bindings for the node binary.
//! Change these to swap implementations at compile time.

use vprogs_storage_rocksdb_store::{DefaultConfig, RocksDbStore};

pub type Vm = vprogs_node_vm::VM;
pub type Store = RocksDbStore<DefaultConfig>;
