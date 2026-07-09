use kaspa_hashes::Hash;

/// Static configuration for the batch prover.
#[derive(Clone, Debug)]
pub struct BatchProverConfig {
    /// The SMT lane key this prover settles.
    pub lane_key: Hash,
    /// Covenant id the batch journal binds to.
    pub covenant_id: Hash,
    /// Deposit address every depositing transaction in the bundle must have committed, or
    /// `[0u8; 32]` when the program credits no L1 deposits. Declared by the deployment, never
    /// derived here: proving a batch that credited a deposit elsewhere fails.
    pub deposit_spk_hash: [u8; 32],
}
