//! The lane-proof source the aggregate prover fetches each bundle's final-block proof over.

use kaspa_rpc_core::{GetSeqCommitLaneProofResponse, api::rpc::RpcApi};
use kaspa_wrpc_client::prelude::KaspaRpcClient;
use vprogs_zk_batch_prover::{LaneProofError, LaneProofRequest, LaneProofSource};

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
/// transiently; the prover must ride out a blip rather than fail on the first one. Sized to cover a
/// short node hiccup without wedging the prover indefinitely on a genuinely dead node. An
/// exhausted-retry failure surfaces as `Err`, which the aggregate prover treats as a deferred
/// bundle (dead block or stalled node) rather than a crash.
const LANE_PROOF_MAX_ATTEMPTS: u32 = 10;
/// Delay between lane-proof RPC retries.
const LANE_PROOF_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

impl LaneProofSource for RemoteLaneSource {
    async fn fetch_lane_proof(
        &self,
        req: LaneProofRequest,
    ) -> Result<GetSeqCommitLaneProofResponse, LaneProofError> {
        // Transient wRPC errors (request timeout, dropped connection) are expected against a live
        // node, so retry with backoff. A block a reorg orphaned is never served again, so once the
        // retries are exhausted the fetch fails and the caller defers the bundle until the chain
        // or the node recovers.
        for attempt in 1..=LANE_PROOF_MAX_ATTEMPTS {
            match self.client.get_seq_commit_lane_proof(req.block, req.lane_key).await {
                Ok(proof) => return Ok(proof),
                Err(e) if attempt < LANE_PROOF_MAX_ATTEMPTS => {
                    log::warn!(
                        "get_seq_commit_lane_proof failed (attempt {attempt}/{LANE_PROOF_MAX_ATTEMPTS}, retrying): {e}"
                    );
                    tokio::time::sleep(LANE_PROOF_RETRY_DELAY).await;
                }
                Err(e) => {
                    log::error!(
                        "get_seq_commit_lane_proof failed after {LANE_PROOF_MAX_ATTEMPTS} \
                         attempts: {e}"
                    );
                    return Err(LaneProofError(e.to_string()));
                }
            }
        }
        unreachable!("lane-proof retry loop returns or fails on the final attempt")
    }
}
