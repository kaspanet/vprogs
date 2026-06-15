use kaspa_hashes::Hash;
use tokio::sync::mpsc::UnboundedSender;
use vprogs_zk_batch_prover::LaneProofSource;

use crate::BundleOutcome;

/// Static configuration for the aggregate prover.
///
/// Generic over the lane-proof source `L` so the same worker runs over a live wRPC client in the
/// node and over an in-process consensus handle in the simulation, and over the receipt type `R` so
/// the settlement sink carries the concrete backend receipt. The source is owned (moved into the
/// worker thread), so this config is consumed once by [`AggregateProver::new`].
pub struct AggregateProverConfig<L: LaneProofSource, R> {
    /// The SMT lane key this prover settles. Seeds the lane-proof fetch for each bundle.
    pub lane_key: Hash,
    /// Covenant id this settlement binds to, or `None` to skip the local covenant-id check. The
    /// aggregator journal self-reports the covenant id; this is asserted against it as a sanity
    /// check before a bundle is accepted.
    pub covenant_id: Option<Hash>,
    /// Source of the bundle's final-block lane proof, used to derive the bundle's
    /// `new_seq_commit`.
    pub lane_source: L,
    /// Sink the worker hands each formed bundle's [`BundleOutcome`] to (a settlement to submit, or
    /// a no-op marker), for a settlement worker to act on. `None` runs the prover without
    /// settling (e.g. exec/test paths) — proved bundles are then only logged.
    pub settlement_sink: Option<UnboundedSender<BundleOutcome<R>>>,
}
