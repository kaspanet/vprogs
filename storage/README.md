# storage/

Defines **how** we store. This layer implements the persistence mechanisms using the state definitions from the state layer.

## Crates

### types/
`vprogs-storage-types`

Abstract storage traits that define the persistence interface:

```rust
pub trait ReadStore {
    type StateSpace;
    fn get(&self, state_space: Self::StateSpace, key: &[u8]) -> Option<Vec<u8>>;
}

pub trait WriteBatch {
    type StateSpace;
    fn put(&mut self, state_space: Self::StateSpace, key: &[u8], value: &[u8]);
    fn delete(&mut self, state_space: Self::StateSpace, key: &[u8]);
}

pub trait Store: ReadStore {
    type WriteBatch: WriteBatch<StateSpace = Self::StateSpace>;
    fn write_batch(&self) -> Self::WriteBatch;
    fn commit(&self, write_batch: Self::WriteBatch);
    fn prefix_iter(&self, state_space: Self::StateSpace, prefix: &[u8]) -> PrefixIterator<'_>;
}
```

### manager/
`vprogs-storage-manager`

Coordinates read and write operations through dedicated worker threads:

- **StorageManager** - Central coordination point for all storage operations
- **ReadCmd / WriteCmd** - Command traits for read/write operations
- Background workers process commands asynchronously
- Provides the `concat_bytes!` macro for key construction

### rocksdb-store/
`vprogs-storage-rocksdb-store`

RocksDB implementation of the Store trait:

- Column families for each StateSpace variant
- Configurable compression (lz4, zstd, snappy, zlib, bzip2)
- jemalloc allocator for performance on non-Windows targets

## Layer Position

```
┌─────────────────────────────────────────┐
│  Layer 3: scheduling                    │
├─────────────────────────────────────────┤
│  Layer 2: state                         │
├─────────────────────────────────────────┤
│  Layer 1: storage  ◄── You are here     │
├─────────────────────────────────────────┤
│  Layer 0: core                          │
└─────────────────────────────────────────┘
```

The storage layer implements persistence. The state layer above defines what gets stored. The scheduling layer uses storage to coordinate state access.

## Design Philosophy

Storage is separated from state to:
1. Allow swapping storage backends (RocksDB, in-memory, etc.)
2. Keep persistence concerns isolated from data semantics
3. Enable testing with mock stores
