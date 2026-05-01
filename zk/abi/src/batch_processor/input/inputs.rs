use alloc::vec::Vec;

use kaspa_hashes::Hash;
#[cfg(feature = "host")]
use kaspa_rpc_core::GetSeqCommitLaneProofResponse;
#[cfg(feature = "host")]
use tap::Tap;
use vprogs_core_codec::Reader;
use vprogs_core_smt::proving::Proof;
use zerocopy::FromBytes;

#[cfg(feature = "host")]
use crate::Write;
#[cfg(feature = "host")]
use crate::batch_processor::BundlePart;
use crate::{
    Result,
    batch_processor::{Batch, LaneProof},
};

/// Decoded batch processor input.
pub struct Inputs<'a> {
    /// Transaction processor guest image ID used to verify each inner tx journal.
    pub image_id: &'a [u8; 32],
    /// Covenant id this bundle settles into.
    pub covenant_id: &'a [u8; 32],
    /// Lane key for this bundle (one lane per bundle).
    pub lane_key: &'a Hash,
    /// SMT proof covering the union of resources touched across all batches.
    pub proof: Proof<'a>,
    /// Leaf-order permutation: `leaf_order[leaf_pos] = bundle_resource_index`.
    pub leaf_order: Vec<u32>,
    /// Batches in scheduling order.
    pub batches: Vec<Batch<'a>>,
    /// Lane proof for the bundle's final block.
    pub lane_proof: LaneProof<'a>,
}

impl<'a> Inputs<'a> {
    /// Decodes the bundle input from a raw byte buffer into zero-copy views.
    pub fn decode(mut buf: &'a [u8]) -> Result<Self> {
        Ok(Self {
            image_id: buf.array::<32>("image_id")?,
            covenant_id: buf.array::<32>("covenant_id")?,
            lane_key: Hash::ref_from_bytes(buf.array::<32>("lane_key")?)?,
            proof: Proof::decode(buf.blob("proof")?)?,
            leaf_order: buf.many("leaf_order", |b| b.le_u32("leaf_order"))?,
            batches: buf.many("batches", Batch::decode)?,
            lane_proof: LaneProof::decode(&mut buf)?,
        })
    }

    /// Encodes a bundle input to bytes.
    #[cfg(feature = "host")]
    pub fn encode<'b, I>(
        image_id: &[u8; 32],
        covenant_id: &[u8; 32],
        lane_key: &Hash,
        proof_bytes: &[u8],
        leaf_order: &[u32],
        batches: I,
        lane_proof: &GetSeqCommitLaneProofResponse,
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
            buf.write_many(leaf_order, |&idx| idx.to_le_bytes());
            buf.encode_many(batches, Batch::encode);
            LaneProof::encode(buf, lane_proof);
        })
    }
}
