#[cfg(feature = "host")]
use kaspa_rpc_core::GetSeqCommitLaneProofResponse;
use vprogs_core_codec::{Reader, Result};

#[cfg(feature = "host")]
use crate::Write;

/// Final-block inputs for deriving the bundle's `new_seq_commit`.
pub struct LaneProof<'a> {
    /// Payload-and-context digest for the bundle's final block.
    pub payload_and_ctx_digest: &'a [u8; 32],
    /// Serialized SMT proof of the lane against the final block's `lanes_root`.
    pub lane_smt_proof: &'a [u8],
    /// `seq_commit` of the bundle's final-block selected parent.
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

    /// Encodes a lane proof to bytes.
    #[cfg(feature = "host")]
    pub fn encode(buf: &mut Vec<u8>, response: &GetSeqCommitLaneProofResponse) {
        buf.write(&response.payload_and_ctx_digest.as_bytes());
        buf.write(&response.parent_seq_commit.as_bytes());
        buf.write_blob(&response.smt_proof);
    }
}
