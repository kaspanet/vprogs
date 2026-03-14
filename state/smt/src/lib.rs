//! Persistence adapters for the SMT.
//!
//! Provides `SmtCommit` (batch writer) and `SmtMetadata` (root hash storage) that bridge
//! `core/crypto` SMT types to the storage layer.

mod commit;
mod metadata;

pub use commit::SmtCommit;
pub use metadata::SmtMetadata;
