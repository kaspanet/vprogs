//! Fork-aware canonical chain over monotonic batch ids: a `block_hash`-keyed log with a lock-free
//! `is_canonical(id)` overlay. See `spec.md` at the repo root for the full design.

mod append_outcome;
mod bucket;
mod chain;
mod hot_zone;
mod manager;
mod snapshot;

pub use append_outcome::AppendOutcome;
pub use bucket::CAPACITY as BUCKET_CAPACITY;
pub use chain::CanonicalChain;
pub use manager::CanonicalChainManager;
pub use snapshot::CanonicalChainSnapshot;
