/// Construction parameters for the driver.
pub struct L2Config {
    /// Lane id; selects the lane subnetwork, lane key, and the tracked resource.
    pub lane_id: u32,
    /// Seed for the activity / settlement RNG.
    pub seed: u64,
    /// Max activity transactions issued per block (the actual count is seeded `0..=this`).
    pub activity_per_block: u64,
    /// Bootstrap a covenant and settle it. When false the driver only does activity + execution.
    pub enable_settlements: bool,
    /// Issue a settlement roughly every this-many blocks once the covenant is active (ignored when
    /// settlements are disabled).
    pub settle_every: u64,
    /// Drive the full proving stack (`ProvingPipeline::aggregate`) off the simulation's consensus
    /// instead of running execution-only. With the crate's `cuda` feature the proofs are real
    /// GPU proofs; without it (or under `RISC0_DEV_MODE=1`) the proving machinery still runs
    /// end to end with the CPU/dev executor, which is what makes the wiring testable without a
    /// GPU.
    pub enable_proving: bool,
    /// Batches bundled per proof when `enable_proving` is set (clamped to at least 1).
    pub bundle_size: usize,
}
