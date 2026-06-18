//! Lock-free, growable canonical chain backed by a rolling ring.

mod lock_free_canonical_chain;

pub use lock_free_canonical_chain::LockFreeCanonicalChain;
