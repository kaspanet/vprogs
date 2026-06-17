use kaspa_consensus_core::{mass::units::ComputeBudget, tx::Transaction};

use super::covenant_advance::CovenantAdvance;

/// A production settlement built for one proven bundle: the transaction ready for fee/mass
/// finalization, its minimal sufficient covenant compute budget, and how to advance the covenant
/// once the finalized tx confirms. Built by [`build_settlement`](super::build_settlement); the
/// caller owns submission and confirmation.
pub struct BuiltSettlement {
    /// The settlement transaction, ready to fund a fee and finalize mass before submission.
    pub transaction: Transaction,
    /// Minimal sufficient covenant-input compute budget, sized off the artifact's seq-commit
    /// anchor (an oversized budget inflates the tx's compute mass past the per-tx limit and
    /// the node rejects it).
    pub compute_budget: ComputeBudget,
    /// The covenant state this settlement advances to, pending the finalized tx's txid and
    /// confirmation DAA score.
    pub advance: CovenantAdvance,
}
