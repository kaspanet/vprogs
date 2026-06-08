#[cfg(feature = "host")]
use kaspa_rpc_core::GetSeqCommitLaneProofResponse;
#[cfg(feature = "host")]
use tap::Tap;
use vprogs_core_codec::Reader;
#[cfg(feature = "host")]
use vprogs_core_codec::Writer;

use crate::{
    Result,
    batch_aggregator::{BatchTransitions, LaneProof},
};

/// Decoded batch aggregator input.
pub struct Inputs<'a> {
    /// Per-batch guest image ID used to verify each journal.
    pub batch_image_id: &'a [u8; 32],
    /// Lane proof for the bundle's final block.
    pub lane_proof: LaneProof<'a>,
    /// Per-batch journals (length-prefixed) in scheduling order.
    pub batch_journals: BatchTransitions<'a>,
}

impl<'a> Inputs<'a> {
    /// Decodes the aggregator input from a raw byte buffer into zero-copy views.
    pub fn decode(mut buf: &'a [u8]) -> Result<Self> {
        Ok(Self {
            batch_image_id: buf.array::<32>("batch_image_id")?,
            lane_proof: LaneProof::decode(&mut buf)?,
            batch_journals: BatchTransitions::new(buf),
        })
    }

    /// Encodes an aggregator input to bytes.
    #[cfg(feature = "host")]
    pub fn encode<I: IntoIterator<Item: AsRef<[u8]>>>(
        batch_image_id: &[u8; 32],
        lane_proof: &GetSeqCommitLaneProofResponse,
        batch_journals: I,
    ) -> Vec<u8> {
        Vec::new().tap_mut(|buf| {
            buf.write(batch_image_id);
            LaneProof::encode(buf, lane_proof);
            for journal in batch_journals {
                buf.write_blob(journal.as_ref());
            }
        })
    }
}
