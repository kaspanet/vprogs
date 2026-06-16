use kaspa_hashes::Hash;

/// Static configuration for the batch prover.
#[derive(Clone, Debug)]
pub struct BatchProverConfig {
    /// The SMT lane key this prover settles.
    pub lane_key: Hash,
    /// Covenant id the batch journal binds to, or `None` to commit the all-zero placeholder.
    pub covenant_id: Option<Hash>,
}
