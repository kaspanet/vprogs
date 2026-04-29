use std::num::NonZeroUsize;

use kaspa_hashes::Hash;

/// Static configuration for the batch prover.
///
/// Bundles K consecutive batches into a single proof + single settlement transaction. K=1
/// degenerates to per-batch proving - the same circuit handles both regimes via the
/// batch-loop ABI.
#[derive(Clone, Debug)]
pub struct BatchProverConfig {
    /// How many scheduled batches to bundle into a single bundle proof. Larger bundles
    /// amortize settlement cost; smaller bundles bound the worst-case wasted compute on a
    /// reorg-induced retry.
    pub bundle_size: NonZeroUsize,
    /// Our lane key. Bundle-wide (one lane per prover instance).
    pub lane_key: Hash,
}
