//! The lane-proof source the aggregate prover fetches each bundle's final-block proof over.

use kaspa_rpc_core::{GetSeqCommitLaneProofResponse, api::rpc::RpcApi};
use kaspa_wrpc_client::prelude::KaspaRpcClient;
use vprogs_zk_batch_prover::{LaneProofRequest, LaneProofSource};

/// A [`LaneProofSource`] backed by the remote node's wRPC client: forwards each fetch to the node's
/// `get_seq_commit_lane_proof` RPC. The in-process analogue is `ConsensusLaneSource`, which reads a
/// direct consensus handle instead of going over RPC.
pub struct RemoteLaneSource {
    client: KaspaRpcClient,
}

impl RemoteLaneSource {
    /// Wraps a connected wRPC client (cloned, so the prover's detached worker owns its own handle).
    pub fn new(client: KaspaRpcClient) -> Self {
        Self { client }
    }
}

/// Bounded retries for the lane-proof RPC before giving up. A real testnet node times out
/// transiently; the prover must ride out a blip rather than die on the first one. Sized to cover a
/// short node hiccup without wedging the prover indefinitely on a genuinely dead node.
const LANE_PROOF_MAX_ATTEMPTS: u32 = 10;
/// Delay between lane-proof RPC retries.
const LANE_PROOF_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

impl LaneProofSource for RemoteLaneSource {
    async fn fetch_lane_proof(&self, req: LaneProofRequest) -> GetSeqCommitLaneProofResponse {
        // Transient wRPC errors (request timeout, dropped connection) are expected against a live
        // node, so retry with backoff instead of crashing the prover worker. The trait yields a
        // plain response, so an exhausted-retry failure can only surface as a panic.
        for attempt in 1..=LANE_PROOF_MAX_ATTEMPTS {
            match self.client.get_seq_commit_lane_proof(req.block, req.lane_key).await {
                Ok(proof) => return proof,
                Err(e) if attempt < LANE_PROOF_MAX_ATTEMPTS => {
                    log::warn!(
                        "get_seq_commit_lane_proof failed (attempt {attempt}/{LANE_PROOF_MAX_ATTEMPTS}, retrying): {e}"
                    );
                    tokio::time::sleep(LANE_PROOF_RETRY_DELAY).await;
                }
                Err(e) => panic!(
                    "get_seq_commit_lane_proof failed after {LANE_PROOF_MAX_ATTEMPTS} attempts: {e}"
                ),
            }
        }
        unreachable!("lane-proof retry loop returns or panics on the final attempt")
    }
}
