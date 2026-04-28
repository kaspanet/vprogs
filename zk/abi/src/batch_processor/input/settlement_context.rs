use vprogs_core_codec::{Reader, Result};

/// Final-block ingredients used once per bundle to derive the settlement journal's
/// `new_seq_commit`.
///
/// Replaces the per-block `miner_payload_leaves` + in-guest `miner_payload_root` +
/// `payload_and_context_digest` derivation with a pre-formed `payload_and_ctx_digest`
/// supplied by the kaspa node's `get_seq_commit_lane_proof` RPC (PR #961). The covenant's
/// `OpChainblockSeqCommit(block_prove_to)` check then transitively pins the entire bundle's
/// lane-tip chain via collision-resistance of `lane_tip_next`.
pub struct SettlementContext<'a> {
    /// Pre-formed `payload_and_context_digest(context_hash, miner_payload_root)` for the
    /// bundle's final block. Comes directly from the RPC response.
    pub payload_and_ctx_digest: &'a [u8; 32],
    /// Serialized `kaspa_smt::proof::OwnedSmtProof` for `lane_key` against the final block's
    /// post-update `lanes_root`.
    pub lane_smt_proof: &'a [u8],
    /// `seq_commit` of the bundle's final-block selected parent — the `H_seq` chain input
    /// for kip21's `seq_commit = H_seq(parent_seq_commit, state_root)`.
    pub parent_seq_commit: &'a [u8; 32],
}

impl<'a> SettlementContext<'a> {
    /// Decodes a `SettlementContext` from a wire buffer.
    ///
    /// Wire layout: `payload_and_ctx_digest(32) | parent_seq_commit(32) | lane_smt_proof_len(u32
    /// LE) | lane_smt_proof bytes`.
    pub fn decode(buf: &mut &'a [u8]) -> Result<Self> {
        Ok(Self {
            payload_and_ctx_digest: buf.array::<32>("payload_and_ctx_digest")?,
            parent_seq_commit: buf.array::<32>("parent_seq_commit")?,
            lane_smt_proof: buf.blob("lane_smt_proof")?,
        })
    }

    /// Encodes a `SettlementContext` to bytes (host-side). Reads the fields directly off
    /// the kaspa `GetSeqCommitLaneProofResponse` — same pattern as `Batch::encode`
    /// reading off `ChainBlockMetadata`.
    #[cfg(feature = "host")]
    pub fn encode(buf: &mut Vec<u8>, response: &kaspa_rpc_core::GetSeqCommitLaneProofResponse) {
        use crate::Write;

        buf.write(&response.payload_and_ctx_digest.as_bytes());
        buf.write(&response.parent_seq_commit.as_bytes());
        buf.write(&(response.smt_proof.len() as u32).to_le_bytes());
        buf.write(&response.smt_proof);
    }
}
