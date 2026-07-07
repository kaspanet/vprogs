//! Framework-agnostic snapshot container: frames an opaque header plus a stream of
//! (resource_id, value) records into a digest-sealed blob, and rebuilds the authenticated
//! SMT root from those records. Callers own the header's meaning.

mod container;

pub use container::{
    FORMAT_VERSION, MAGIC, Record, SnapshotError, read_container, write_container,
};
