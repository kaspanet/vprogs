//! Framework-agnostic snapshot codec: streams an opaque header plus a sequence of (resource_id,
//! value) records into a blob with a trailing digest. Callers own the header's meaning and the
//! framing identity (magic and format version, via [`SnapshotFormat`]); the digest is generic over
//! the caller's [`vprogs_core_hashing::Hasher`], so a program using a non-default framework hasher
//! is represented faithfully.
//!
//! Neither side ever holds the whole file in memory: [`SnapshotWriter`] is a push-style writer
//! that streams borrowed record slices straight to the output, and [`SnapshotReader`] is a lending
//! reader that yields one borrowed record at a time.
//!
//! The trailing digest is an integrity check only: it guards against accidental corruption in
//! transit or at rest, and is recomputed by any producer, so it authenticates nothing about the
//! contents. Reconstructing and authenticating a snapshot's SMT root from its records is the
//! caller's responsibility (e.g. via `vprogs_core_smt::StreamingBuilder` fed the records this
//! crate yields); this crate has no SMT/proof-system knowledge of its own.

mod container;

pub use container::{
    MAX_HEADER_LEN, MAX_VALUE_LEN, Record, SnapshotError, SnapshotFormat, SnapshotReader,
    SnapshotWriter,
};
