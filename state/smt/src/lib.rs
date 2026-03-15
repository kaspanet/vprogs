//! Persistence adapters for the SMT.
//!
//! Provides `SmtCommit` (batch writer) that bridges `core/crypto` SMT types to the storage layer.

mod commit;

pub use commit::SmtCommit;
