use vprogs_core_codec::{Reader, Result};

/// Final-block ingredients used once per bundle to derive the settlement journal's
/// `new_seq_commit`. The covenant's `OpChainblockSeqCommit(block_prove_to)` check pins
/// `new_seq_commit` to L1, which transitively pins the entire bundle's lane-tip chain
/// via collision-resistance of `lane_tip_next`.
pub struct LaneProof<'a> {
    /// Pre-formed `payload_and_context_digest(context_hash, miner_payload_root)` for the
    /// bundle's final block.
    pub payload_and_ctx_digest: &'a [u8; 32],
    /// Serialized `kaspa_smt::proof::OwnedSmtProof` for `lane_key` against the final block's
    /// post-update `lanes_root`.
    pub lane_smt_proof: &'a [u8],
    /// `seq_commit` of the bundle's final-block selected parent — the `H_seq` chain input
    /// for `seq_commit = H_seq(parent_seq_commit, state_root)`.
    pub parent_seq_commit: &'a [u8; 32],
}

impl<'a> LaneProof<'a> {
    /// Decodes a `LaneProof` from a wire buffer.
    pub fn decode(buf: &mut &'a [u8]) -> Result<Self> {
        Ok(Self {
            payload_and_ctx_digest: buf.array::<32>("payload_and_ctx_digest")?,
            parent_seq_commit: buf.array::<32>("parent_seq_commit")?,
            lane_smt_proof: buf.blob("lane_smt_proof")?,
        })
    }

    /// Encodes a `LaneProof` to bytes directly from the kaspa `GetSeqCommitLaneProofResponse`.
    #[cfg(feature = "host")]
    pub fn encode(buf: &mut Vec<u8>, response: &kaspa_rpc_core::GetSeqCommitLaneProofResponse) {
        use crate::Write;

        buf.write(&response.payload_and_ctx_digest.as_bytes());
        buf.write(&response.parent_seq_commit.as_bytes());
        buf.write(&(response.smt_proof.len() as u32).to_le_bytes());
        buf.write(&response.smt_proof);
    }
}
