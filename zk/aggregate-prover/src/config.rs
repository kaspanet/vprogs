use std::ops::RangeInclusive;

use kaspa_hashes::Hash;
use tokio::sync::watch;
use vprogs_core_atomics::AsyncQueue;
use vprogs_l1_types::SettlementInfo;
use vprogs_zk_batch_prover::LaneProofSource;

use crate::{ScheduledBundle, SettlementArtifact};

/// Static configuration for the aggregate prover.
///
/// Generic over the lane-proof source `L` so the same worker runs over a live wRPC client in the
/// node and over an in-process consensus handle in the simulation, and over the receipt type `R` so
/// the settlement queue carries the concrete backend receipt. The source is owned (moved into the
/// worker thread), so this config is consumed once by [`AggregateProver::new`].
pub struct AggregateProverConfig<L: LaneProofSource, R: Send + Sync + 'static> {
    /// The SMT lane key this prover settles. Seeds the lane-proof fetch for each bundle.
    pub lane_key: Hash,
    /// Covenant id this settlement binds to, or `None` to skip the covenant-id check.
    pub covenant_id: Option<Hash>,
    /// Source of the bundle's final-block lane proof, used to derive the bundle's
    /// `new_seq_commit`.
    pub lane_source: L,
    /// Queue the worker publishes each formed bundle's [`ScheduledBundle`] handle onto (pushed
    /// before its proof exists; the consumer awaits the artifact), for a settlement worker to act
    /// on. `None` runs the prover without settling (e.g. exec/test paths); proved bundles are
    /// then only logged.
    pub settlement_queue: Option<AsyncQueue<ScheduledBundle<SettlementArtifact<R>>>>,
    /// Receiver on the bridge's covenant `last_settlement` watch driving re-aggregation of a
    /// suffix a competitor superseded, or `None` to run without re-forming.
    pub settlement: Option<watch::Receiver<Option<SettlementInfo>>>,
    /// Inclusive bound on how many scheduled batches one bundle may consume. A bundle forms only
    /// once at least `*start()` batches are consecutively ready (parking until then) and is
    /// capped at `*end()`. `1..=usize::MAX` ("1..") is the greedy default: form as soon as the
    /// front is ready and extend over every consecutively-ready batch.
    pub bundle_size: RangeInclusive<usize>,
}
