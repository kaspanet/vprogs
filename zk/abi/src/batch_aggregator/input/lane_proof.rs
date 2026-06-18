use kaspa_hashes::Hash;
#[cfg(feature = "host")]
use kaspa_rpc_core::GetSeqCommitLaneProofResponse;
use kaspa_smt::{
    proof::{ProofTerminal, SmtProof},
    store::CollapsedLeaf,
};
#[cfg(feature = "host")]
use vprogs_core_codec::Writer;
use vprogs_core_codec::{Error, Reader, Result};
use zerocopy::TryFromBytes;

/// Final-block inputs for deriving the bundle's `new_seq_commit`.
pub struct LaneProof<'a> {
    /// Payload-and-context digest for the bundle's final block.
    pub payload_and_ctx_digest: &'a Hash,
    /// `seq_commit` of the bundle's final-block selected parent.
    pub prev_seq_commit: &'a Hash,
    /// Inactivity shortcut wrapping `lanes_root` into the activity root (post-hardening).
    pub inactivity_shortcut: &'a Hash,
    /// SMT proof of the lane against the final block's `lanes_root`.
    pub lane_smt_proof: SmtProof<'a>,
}

impl<'a> LaneProof<'a> {
    /// Decodes a `LaneProof` from a wire buffer.
    pub fn decode(buf: &mut &'a [u8]) -> Result<Self> {
        Ok(Self {
            payload_and_ctx_digest: buf.array_as::<Hash>("payload_and_ctx_digest")?,
            prev_seq_commit: buf.array_as::<Hash>("prev_seq_commit")?,
            inactivity_shortcut: buf.array_as::<Hash>("inactivity_shortcut")?,
            lane_smt_proof: Self::decode_smt_proof(buf.blob("lane_smt_proof")?)?,
        })
    }

    /// Decodes the lane SMT proof bytes into a borrowed [`SmtProof`].
    fn decode_smt_proof(mut buf: &'a [u8]) -> Result<SmtProof<'a>> {
        Ok(SmtProof {
            bitmap: buf.array::<32>("bitmap")?,
            terminal: match buf.byte("tag")? {
                ProofTerminal::FULL_TAG => ProofTerminal::Full,
                ProofTerminal::COLLAPSED_TAG => {
                    ProofTerminal::Collapsed { depth: buf.byte("depth")? }
                }
                ProofTerminal::COLLAPSED_OTHER_TAG => ProofTerminal::CollapsedOther {
                    depth: buf.byte("depth")?,
                    leaf: CollapsedLeaf {
                        lane_key: *buf.array_as::<Hash>("lane_key")?,
                        leaf_hash: *buf.array_as::<Hash>("leaf_hash")?,
                    },
                },
                _ => return Err(Error::Decode("tag")),
            },
            siblings: <[Hash]>::try_ref_from_bytes(buf)?,
        })
    }

    /// Encodes a lane proof to bytes.
    #[cfg(feature = "host")]
    pub fn encode(buf: &mut impl Writer, response: &GetSeqCommitLaneProofResponse) {
        buf.write(response.payload_and_ctx_digest.as_slice());
        buf.write(response.parent_seq_commit.as_slice());
        buf.write(response.inactivity_shortcut.as_slice());
        buf.write_blob(&response.smt_proof);
    }
}
