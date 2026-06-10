use kaspa_hashes::Hash;

/// Static configuration for the batch prover.
///
/// The prover produces one ZK receipt per scheduled batch. Each receipt commits a
/// [`BatchTransition`] that the (out-of-band) aggregator chains into a bundle settlement.
///
/// [`BatchTransition`]: vprogs_zk_abi::batch_processor::BatchTransition
#[derive(Clone, Debug)]
pub struct BatchProverConfig {
    /// The lane this prover settles, as the 32-byte SMT lane key the guest commits and the
    /// covenant SPK pins; per-prover-instance (one lane per prover).
    pub lane_key: Hash,
    /// Covenant id the produced batch journal binds to. The on-chain settlement script (which
    /// runs against the aggregator's output, not the per-batch journal) reconstructs the
    /// preimage with the input's `OpInputCovenantId`, so every batch's committed `covenant_id`
    /// must equal the deployed covenant UTXO's id. `None` for non-settling / mock-outpoint paths
    /// commits the all-zero placeholder.
    pub covenant_id: Option<Hash>,
}
