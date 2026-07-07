//! Framework-agnostic snapshot container: frames an opaque header plus a stream of
//! (resource_id, value) records into a blob with a trailing SHA-256 digest, and rebuilds the
//! authenticated SMT root from those records. Callers own the header's meaning.
//!
//! The trailing digest is an integrity check only: it guards against accidental corruption in
//! transit or at rest, and is recomputed by any producer, so it authenticates nothing about the
//! contents. A snapshot's contents are authenticated only by recomputing the SMT root from its
//! records (see [`compute_root_from_records`]) and checking that root against the node's
//! committed on-chain settlement root; a file with a correct digest but forged or stale records
//! will fail that check.

mod container;
mod rebuild;

pub use container::{
    FORMAT_VERSION, MAGIC, MAX_SNAPSHOT_BYTES, Record, SnapshotError, read_container,
    write_container,
};
pub use rebuild::{commitments_from_records, compute_root_from_records};
