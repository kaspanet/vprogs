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

/// Bundle-constant values the prover declares for a batch, mirroring the leading fields of
/// [`Inputs`].
pub struct BatchPins<'a> {
    /// See [`Inputs::tx_image_id`].
    pub tx_image_id: &'a [u8; 32],
    /// See [`Inputs::covenant_id`].
    pub covenant_id: &'a [u8; 32],
    /// See [`Inputs::deposit_spk_hash`].
    pub deposit_spk_hash: &'a [u8; 32],
    /// See [`Inputs::lane_key`].
    pub lane_key: &'a Hash,
}

/// Decoded batch processor input.
pub struct Inputs<'a> {
    /// Transaction processor guest image ID used to verify each inner tx journal.
    pub tx_image_id: &'a [u8; 32],
    /// Covenant id this bundle settles into.
    pub covenant_id: &'a [u8; 32],
    /// Deposit address every depositing tx in this bundle must have committed, or `[0u8; 32]` when
    /// the program credits no L1 deposits.
    pub deposit_spk_hash: &'a [u8; 32],
    /// Lane key of the lane this bundle settles.
    pub lane_key: &'a Hash,
    /// SMT proof covering the resources touched in this batch.
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
            deposit_spk_hash: buf.array::<32>("deposit_spk_hash")?,
            lane_key: buf.array_as::<Hash>("lane_key")?,
            proof: Proof::decode(buf.blob("proof")?)?,
            batch: Batch::decode(&mut buf)?,
        })
    }

    /// Encodes a batch processor input to bytes.
    #[cfg(feature = "host")]
    pub fn encode(
        pins: BatchPins<'_>,
        proof_bytes: &[u8],
        metadata: &ChainBlockMetadata,
        tx_journals: &[Vec<u8>],
    ) -> Vec<u8> {
        Vec::new().tap_mut(|buf| {
            buf.write(pins.tx_image_id);
            buf.write(pins.covenant_id);
            buf.write(pins.deposit_spk_hash);
            buf.write(pins.lane_key.as_slice());
            buf.write_blob(proof_bytes);
            Batch::encode(buf, metadata, tx_journals);
        })
    }
}
