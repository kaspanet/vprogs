//! Framework-agnostic snapshot codec: streams an opaque header plus a sequence of (resource_id,
//! value) records into a blob with a trailing digest, and rebuilds the authenticated SMT root
//! from those records. Callers own the header's meaning; the digest and the SMT leaf hash are
//! both generic over the caller's [`vprogs_core_hashing::Hasher`], so a program using a
//! non-default framework hasher is represented faithfully.
//!
//! Neither side ever holds the whole file in memory: [`write_snapshot`] streams records straight
//! to the writer, and [`SnapshotReader`] yields one record at a time.
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
    FORMAT_VERSION, MAGIC, MAX_HEADER_LEN, MAX_VALUE_LEN, Record, SnapshotError, SnapshotReader,
    write_snapshot,
};
pub use rebuild::{commitments_from_records, compute_root_from_records};
