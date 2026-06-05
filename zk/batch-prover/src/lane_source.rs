use std::future::Future;

use kaspa_grpc_client::GrpcClient;
use kaspa_hashes::Hash;
use kaspa_rpc_core::{GetSeqCommitLaneProofResponse, api::rpc::RpcApi};

/// Source of a block's seq-commit lane proof, fetched once per bundle to derive the bundle's
/// `new_seq_commit`. The production node implements this over its wRPC [`GrpcClient`]; an
/// in-process driver (e.g. the simulation) implements it directly over a consensus handle, so the
/// batch prover is not tied to a live RPC connection.
pub trait LaneProofSource: Send + 'static {
    /// Fetches the lane proof for `block` against `lane_key`.
    fn fetch_lane_proof(
        &self,
        block: Hash,
        lane_key: Hash,
    ) -> impl Future<Output = GetSeqCommitLaneProofResponse>;
}

impl LaneProofSource for GrpcClient {
    async fn fetch_lane_proof(&self, block: Hash, lane_key: Hash) -> GetSeqCommitLaneProofResponse {
        self.get_seq_commit_lane_proof(block, lane_key).await.expect("get_seq_commit_lane_proof")
    }
}
