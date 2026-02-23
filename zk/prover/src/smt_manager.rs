//! In-memory SMT manager, updated from committed state.
//!
//! Maintains the host-side Sparse Merkle Tree and produces roots and proofs
//! for the stitcher.

use vprogs_zk_core::smt::Smt;

use crate::BatchEffects;

/// Manages the in-memory SMT, applying batch effects to keep it current.
pub struct SmtManager {
    smt: Smt,
}

impl SmtManager {
    /// Create a new manager with an empty SMT.
    pub fn new() -> Self {
        Self { smt: Smt::new() }
    }

    /// Apply a batch's effects to the SMT.
    ///
    /// For each transaction, inserts or updates the final post_hash for every
    /// written resource. Read-only resources are skipped since their state
    /// hasn't changed.
    pub fn apply_batch(&mut self, batch: &BatchEffects) {
        for tx in &batch.tx_effects {
            for effect in &tx.effects {
                // Only writes change state — skip reads.
                if effect.access_type == 1 {
                    self.smt.upsert(effect.resource_id_hash, effect.post_hash);
                }
            }
        }
    }

    /// Current SMT root.
    pub fn root(&self) -> [u8; 32] {
        self.smt.root()
    }

    /// Access the underlying SMT (e.g. for proof generation).
    pub fn smt(&self) -> &Smt {
        &self.smt
    }
}

impl Default for SmtManager {
    fn default() -> Self {
        Self::new()
    }
}
