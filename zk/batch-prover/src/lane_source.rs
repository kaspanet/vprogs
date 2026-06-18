use std::future::Future;

use kaspa_grpc_client::GrpcClient;
use kaspa_hashes::Hash;
use kaspa_rpc_core::{GetSeqCommitLaneProofResponse, api::rpc::RpcApi};

/// A request for a block's seq-commit lane proof.
#[derive(Clone, Copy)]
pub struct LaneProofRequest {
    /// Block whose lane proof to fetch.
    pub block: Hash,
    /// Lane key the proof is taken against.
    pub lane_key: Hash,
}

/// Source of a block's seq-commit lane proof, fetched once per bundle to derive the bundle's
/// `new_seq_commit`. The production node implements this over its wRPC [`GrpcClient`]; an
/// in-process driver (e.g. the simulation) implements it directly over a consensus handle, so the
/// batch prover is not tied to a live RPC connection.
pub trait LaneProofSource: Send + 'static {
    /// Fetches the lane proof for `req.block` against `req.lane_key`.
    fn fetch_lane_proof(
        &self,
        req: LaneProofRequest,
    ) -> impl Future<Output = GetSeqCommitLaneProofResponse>;
}

impl LaneProofSource for GrpcClient {
    async fn fetch_lane_proof(&self, req: LaneProofRequest) -> GetSeqCommitLaneProofResponse {
        self.get_seq_commit_lane_proof(req.block, req.lane_key)
            .await
            .expect("get_seq_commit_lane_proof")
    }
}
