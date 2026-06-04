use kaspa_hashes::Hash;
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
///
/// The aggregator chains a sequence of [`BatchTransition`] journals into a bundle settlement.
/// Each entry in `batch_journals` is the journal bytes the per-batch guest committed; the
/// aggregator runs `env::verify(batch_image_id, &journal_bytes)` on each, decodes the header,
/// asserts the chain conditions across the sequence, and streams the trailing exits into the
/// permission tree.
///
/// [`BatchTransition`]: crate::batch_processor::BatchTransition
pub struct Inputs<'a> {
    /// Per-batch guest image ID used to verify each [`BatchTransition`] journal.
    ///
    /// [`BatchTransition`]: crate::batch_processor::BatchTransition
    pub batch_image_id: &'a [u8; 32],
    /// Lane proof for the bundle's final block.
    pub lane_proof: LaneProof<'a>,
    /// Per-batch journals (length-prefixed) in scheduling order.
    pub batch_journals: BatchTransitions<'a>,
}

impl<'a> Inputs<'a> {
    /// Decodes the aggregator input from a raw byte buffer into zero-copy views.
    pub fn decode(mut buf: &'a [u8]) -> Result<Self> {
        let batch_image_id = buf.array::<32>("batch_image_id")?;
        let lane_proof = LaneProof::decode(&mut buf)?;
        Ok(Self { batch_image_id, lane_proof, batch_journals: BatchTransitions::new(buf) })
    }

    /// Encodes an aggregator input to bytes.
    #[cfg(feature = "host")]
    pub fn encode<I>(
        batch_image_id: &[u8; 32],
        lane_proof: &GetSeqCommitLaneProofResponse,
        batch_journals: I,
    ) -> Vec<u8>
    where
        I: IntoIterator,
        I::Item: AsRef<[u8]>,
    {
        Vec::new().tap_mut(|buf| {
            buf.write(batch_image_id);
            LaneProof::encode(buf, lane_proof);
            for journal in batch_journals {
                let bytes = journal.as_ref();
                buf.write(&(bytes.len() as u32).to_le_bytes());
                buf.write(bytes);
            }
        })
    }

    /// Returns the lane proof's parent `seq_commit`. The aggregator's [`StateTransition`] commits
    /// the lane proof's `prev_seq_commit` indirectly via the new `seq_commit`; callers that need
    /// the raw value (settlement, debugging) read it through here.
    ///
    /// [`StateTransition`]: crate::batch_aggregator::StateTransition
    pub fn prev_seq_commit(&self) -> &'a Hash {
        self.lane_proof.prev_seq_commit
    }
}
