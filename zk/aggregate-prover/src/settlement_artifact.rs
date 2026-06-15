use kaspa_hashes::Hash;

/// A proven settlement bundle the aggregate prover hands off to a settlement worker.
///
/// Carries the aggregate (settlement-level) receipt plus the bundle's decoded state-transition
/// bounds, so the consumer can build and submit the on-chain settlement without re-proving or
/// re-parsing the journal. The aggregate prover only emits these for bundles that advanced the L2
/// state (no-op bundles are dropped), so every artifact corresponds to a settlement worth landing.
pub struct SettlementArtifact<R> {
    /// Aggregate receipt proving the bundle's state transition, verified on chain by the
    /// covenant's `OpZkPrecompile`.
    pub receipt: R,
    /// L1 block the proof commits to (the bundle's final block).
    pub block_prove_to: Hash,
    /// L2 SMT state root the bundle chains from (must match the live covenant's state).
    pub prev_state: [u8; 32],
    /// Lane tip entering the bundle (must match the spent covenant's redeem prefix).
    pub prev_lane_tip: Hash,
    /// L2 SMT state root after the bundle.
    pub new_state: [u8; 32],
    /// Lane tip after the bundle.
    pub new_lane_tip: Hash,
    /// Block-header `seq_commit` derived from `new_lane_tip` and the lane proof; sizes the
    /// covenant input's compute budget.
    pub new_seq_commit: Hash,
    /// Permission-tree exit-output hash, or all-zero when the bundle emitted no exits.
    pub permission_spk_hash: [u8; 32],
    /// Covenant id the bundle binds to.
    pub covenant_id: [u8; 32],
}

/// The outcome of one bundle the aggregate prover formed, handed to the settlement sink.
///
/// Every formed bundle produces exactly one outcome, so a consumer can account for all batches it
/// submitted (the `batches` of all outcomes sum to the number of batches scheduled). A consumer
/// that only settles can ignore `batches` and act on `settlement`; a consumer that paces itself
/// against proving (e.g. the simulation) uses `batches` to know how many of its submitted batches a
/// given outcome covered, so it can block for the next outcome without deadlocking on a no-op
/// bundle.
pub struct BundleOutcome<R> {
    /// Number of scheduled batches this bundle consumed (including empty batches in the ready
    /// prefix).
    pub batches: usize,
    /// The settlement to submit, or `None` when the bundle advanced no state (a no-op or all-empty
    /// bundle).
    pub settlement: Option<SettlementArtifact<R>>,
}
