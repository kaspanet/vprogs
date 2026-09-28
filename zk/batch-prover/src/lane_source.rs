use std::future::Future;

use kaspa_hashes::Hash;
use kaspa_rpc_core::GetSeqCommitLaneProofResponse;

/// A request for a block's seq-commit lane proof.
#[derive(Clone, Copy)]
pub struct LaneProofRequest {
    /// Block whose lane proof to fetch.
    pub block: Hash,
    /// Lane key the proof is taken against.
    pub lane_key: Hash,
}

/// A lane-proof fetch that failed: the block may be dead (reorged away, so the node no longer
/// serves it) or the node may be unreachable past the source's retries.
#[derive(Debug)]
pub struct LaneProofError(pub String);

impl std::fmt::Display for LaneProofError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LaneProofError {}

/// Source of a block's seq-commit lane proof, fetched once per bundle to derive the bundle's
/// `new_seq_commit`. An in-process driver (e.g. the simulation) implements it directly over a
/// consensus handle; a remote node implements it over RPC, so the batch prover is not tied to a
/// live connection. The fetch is fallible: a block reorged away mid-aggregation is no longer
/// served, and the aggregate prover defers such a bundle instead of crashing.
pub trait LaneProofSource: Send + 'static {
    /// Fetches the lane proof for `req.block` against `req.lane_key`.
    fn fetch_lane_proof(
        &self,
        req: LaneProofRequest,
    ) -> impl Future<Output = Result<GetSeqCommitLaneProofResponse, LaneProofError>>;
}
