use kaspa_hashes::Hash;
#[cfg(feature = "host")]
use tap::Tap;
use vprogs_core_codec::Reader;
#[cfg(feature = "host")]
use vprogs_core_codec::Writer;
use vprogs_core_smt::proving::Proof;
#[cfg(feature = "host")]
use vprogs_l1_types::ChainBlockMetadata;

use crate::{Result, batch_processor::Batch};

/// Decoded single-batch processor input.
///
/// One of these is produced per scheduled batch; the [`Verifier`] proves the state transition for
/// that batch in isolation, and the [`AggregatorVerifier`] chains a sequence of resulting
/// [`BatchTransition`] journals into a bundle settlement.
///
/// [`Verifier`]: crate::batch_processor::Verifier
/// [`AggregatorVerifier`]: crate::batch_aggregator::AggregatorVerifier
/// [`BatchTransition`]: crate::batch_processor::BatchTransition
pub struct Inputs<'a> {
    /// Transaction processor guest image ID used to verify each inner tx journal.
    pub tx_image_id: &'a [u8; 32],
    /// Covenant id this batch settles into (carried through to the [`BatchTransition`] so the
    /// aggregator asserts every batch shares it).
    pub covenant_id: &'a [u8; 32],
    /// Lane key for this batch's lane (carried through to the [`BatchTransition`] for the
    /// aggregator's same-lane assert and the seq_commit derivation).
    pub lane_key: &'a Hash,
    /// SMT proof scoped to the resources this batch touches (one proof per batch, smaller than
    /// the bundle-wide proof the monolithic guest used to take).
    pub proof: Proof<'a>,
    /// The batch's per-block context and tx journals.
    pub batch: Batch<'a>,
}

impl<'a> Inputs<'a> {
    /// Decodes the batch input from a raw byte buffer into zero-copy views.
    pub fn decode(mut buf: &'a [u8]) -> Result<Self> {
        Ok(Self {
            tx_image_id: buf.array::<32>("tx_image_id")?,
            covenant_id: buf.array::<32>("covenant_id")?,
            lane_key: buf.array_as::<Hash>("lane_key")?,
            proof: Proof::decode(buf.blob("proof")?)?,
            batch: Batch::decode(&mut buf)?,
        })
    }

    /// Encodes a single-batch input to bytes.
    ///
    /// The first tuple groups the per-batch invariants that the [`BatchTransition`] header
    /// carries through to the aggregator: `(tx_image_id, covenant_id, lane_key)`.
    ///
    /// [`BatchTransition`]: crate::batch_processor::BatchTransition
    #[cfg(feature = "host")]
    pub fn encode(
        (tx_image_id, covenant_id, lane_key): (&[u8; 32], &[u8; 32], &Hash),
        proof_bytes: &[u8],
        metadata: &ChainBlockMetadata,
        tx_journals: &[Vec<u8>],
    ) -> Vec<u8> {
        Vec::new().tap_mut(|buf| {
            buf.write(tx_image_id);
            buf.write(covenant_id);
            buf.write(lane_key.as_slice());
            buf.write_blob(proof_bytes);
            Batch::encode(buf, metadata, tx_journals);
        })
    }
}
