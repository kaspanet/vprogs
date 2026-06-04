use kaspa_hashes::Hash;
#[cfg(feature = "host")]
use kaspa_rpc_core::GetSeqCommitLaneProofResponse;
#[cfg(feature = "host")]
use tap::Tap;
use vprogs_core_codec::Reader;
#[cfg(feature = "host")]
use vprogs_core_codec::Writer;
use vprogs_core_smt::proving::Proof;
#[cfg(feature = "host")]
use zerocopy::IntoBytes;
use zerocopy::{FromBytes, little_endian::U32};

#[cfg(feature = "host")]
use crate::batch_processor::{Batch, BundlePart};
use crate::{
    Result,
    batch_processor::{Batches, LaneProof},
};

/// Decoded batch processor input.
pub struct Inputs<'a> {
    /// Transaction processor guest image ID used to verify each inner tx journal.
    pub image_id: &'a [u8; 32],
    /// Covenant id this bundle settles into.
    pub covenant_id: &'a [u8; 32],
    /// Lane key of the lane this bundle settles.
    pub lane_key: &'a Hash,
    /// SMT proof covering the union of resources touched across all batches.
    pub proof: Proof<'a>,
    /// Leaf-order permutation: `leaf_order[leaf_pos] = bundle_resource_index`.
    pub leaf_order: &'a [U32],
    /// Lane proof for the bundle's final block.
    pub lane_proof: LaneProof<'a>,
    /// Batches in scheduling order.
    pub batches: Batches<'a>,
}

impl<'a> Inputs<'a> {
    /// Decodes the bundle input from a raw byte buffer into zero-copy views.
    pub fn decode(mut buf: &'a [u8]) -> Result<Self> {
        Ok(Self {
            image_id: buf.array::<32>("image_id")?,
            covenant_id: buf.array::<32>("covenant_id")?,
            lane_key: buf.array_as::<Hash>("lane_key")?,
            proof: Proof::decode(buf.blob("proof")?)?,
            leaf_order: <[U32]>::ref_from_bytes(buf.blob("leaf_order")?)?,
            lane_proof: LaneProof::decode(&mut buf)?,
            batches: Batches::decode(buf)?,
        })
    }

    /// Encodes a bundle input to bytes.
    #[cfg(feature = "host")]
    pub fn encode<'b, I>(
        image_id: &[u8; 32],
        covenant_id: &[u8; 32],
        lane_key: &Hash,
        proof_bytes: &[u8],
        leaf_order: &[U32],
        lane_proof: &GetSeqCommitLaneProofResponse,
        batches: I,
    ) -> Vec<u8>
    where
        I: IntoIterator<Item = BundlePart<'b>>,
        I::IntoIter: ExactSizeIterator,
    {
        Vec::new().tap_mut(|buf| {
            buf.write(image_id);
            buf.write(covenant_id);
            buf.write(lane_key.as_slice());
            buf.write_blob(proof_bytes);
            buf.write_blob(leaf_order.as_bytes());
            LaneProof::encode(buf, lane_proof);
            buf.encode_many(batches, Batch::encode);
        })
    }
}
