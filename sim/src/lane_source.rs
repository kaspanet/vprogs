//! A [`LaneProofSource`] backed by an in-process consensus, for driving the batch prover from the
//! simulation instead of a live gRPC node.

use std::sync::Weak;

use kaspa_consensus::consensus::Consensus;
use kaspa_consensus_core::api::ConsensusApi;
use kaspa_rpc_core::{GetSeqCommitLaneProofResponse, RpcLaneEntry};
use vprogs_zk_batch_prover::{LaneProofError, LaneProofRequest, LaneProofSource};

/// Supplies the batch prover with a block's seq-commit lane proof by reading the simulation's
/// consensus directly (the production path uses a `GrpcClient`). This is what lets the cuda/proving
/// sim run without a gRPC server.
///
/// It holds a [`Weak`] handle **deliberately**. The consensus is owned by the simulation's node,
/// whose db lifetime is asserted ("DB has N strong references") when the node is torn down. The
/// batch-prover worker runs on a detached thread that outlives the run, so a strong
/// `Arc<Consensus>` captured here could keep the db alive past teardown and trip that assert. A
/// `Weak` never extends the consensus lifetime; once the node is gone `upgrade` fails and the
/// fetch errors (the aggregate prover defers the bundle; the worker is mid-teardown anyway, so
/// its receipt would be discarded); this is only reachable while shutting down, and failing lets
/// the detached worker's runtime drop silently instead of feeding the guest an undecodable empty
/// SMT proof.
pub struct ConsensusLaneSource {
    consensus: Weak<Consensus>,
}

impl ConsensusLaneSource {
    /// Builds the source from a weak handle on the node's consensus (the driver keeps a [`Weak`] so
    /// the source never extends the consensus lifetime; pass `Arc::downgrade(&consensus)`).
    pub fn from_weak(consensus: Weak<Consensus>) -> Self {
        Self { consensus }
    }
}

impl LaneProofSource for ConsensusLaneSource {
    async fn fetch_lane_proof(
        &self,
        LaneProofRequest { block, lane_key }: LaneProofRequest,
    ) -> Result<GetSeqCommitLaneProofResponse, LaneProofError> {
        let Some(consensus) = self.consensus.upgrade() else {
            // The node was torn down (we are shutting down); the error defers this bundle instead
            // of parking the fetch inside a torn-down consensus.
            return Err(LaneProofError("consensus torn down during shutdown".into()));
        };
        // Mirrors the RPC service's conversion (rpc/service/src/service.rs at the pinned commit):
        // the only non-trivial field is the SMT proof, serialized to bytes.
        let proof = consensus
            .get_seq_commit_lane_proof(block, lane_key)
            .map_err(|e| LaneProofError(format!("get_seq_commit_lane_proof: {e}")))?;
        Ok(GetSeqCommitLaneProofResponse {
            smt_proof: proof.smt_proof.to_bytes(),
            lane: proof.lane.map(|l| RpcLaneEntry { tip: l.tip, blue_score: l.blue_score }),
            payload_and_ctx_digest: proof.payload_and_ctx_digest,
            parent_seq_commit: proof.parent_seq_commit,
            inactivity_shortcut: proof.inactivity_shortcut,
        })
    }
}
