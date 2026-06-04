use kaspa_hashes::Hash;
use vprogs_core_codec::Writer;
use zerocopy::{FromBytes, Immutable, KnownLayout, Unaligned};

use crate::transaction_processor::ExitCommitment;

/// Per-batch state-transition journal. The single-batch [`Verifier`] commits one of these per
/// batch; the [`AggregatorVerifier`] verifies a sequence of them via `env::verify`, chains them
/// (each batch's `new_*` must equal the next batch's `prev_*`), streams the trailing exits into
/// the permission tree, and folds the chained extremes into the bundle's [`StateTransition`].
///
/// The journal is laid out as a fixed zerocopy header followed by a length-prefixed exits blob:
/// the header carries the chain anchors the aggregator asserts on, the exits blob carries the
/// per-tx emissions the aggregator streams into the permission tree.
///
/// [`Verifier`]: crate::batch_processor::Verifier
/// [`AggregatorVerifier`]: crate::batch_aggregator::AggregatorVerifier
/// [`StateTransition`]: crate::batch_aggregator::StateTransition
#[repr(C)]
#[derive(FromBytes, Immutable, KnownLayout, Unaligned)]
pub struct BatchTransition {
    /// L2 SMT state root before this batch.
    pub prev_state: [u8; 32],
    /// L2 SMT state root after this batch.
    pub new_state: [u8; 32],
    /// Lane tip entering this batch's block.
    pub prev_lane_tip: Hash,
    /// Lane tip after this batch (carried forward unchanged on empty batches).
    pub new_lane_tip: Hash,
    /// Blue score at which the lane was last active before this batch.
    pub prev_lane_blue_score: zerocopy::little_endian::U64,
    /// Blue score at which the lane was last active after this batch (carried forward unchanged
    /// on empty batches).
    pub new_lane_blue_score: zerocopy::little_endian::U64,
    /// Hash of the lane's subnetwork id; aggregator asserts every batch in a bundle shares the
    /// same `lane_key`.
    pub lane_key: Hash,
    /// Covenant id this batch settles into; aggregator asserts every batch shares it.
    pub covenant_id: [u8; 32],
    /// Transaction-processor image id this batch was verified against; aggregator asserts every
    /// batch shares it.
    pub tx_image_id: [u8; 32],
    /// `1` when the lane re-anchored on the chain block's `prev_seq_commit` instead of
    /// `prev_lane_tip` (aggregator skips the `lane_tip` chain check across this boundary).
    pub lane_expired: u8,
}

/// Serialized length of the fixed [`BatchTransition`] header, in bytes. The exits blob follows
/// the header and is read separately via [`exits`](BatchTransition::exits).
pub const HEADER_SIZE: usize = core::mem::size_of::<BatchTransition>();

impl BatchTransition {
    /// Writes a batch transition journal: fixed header followed by a length-prefixed exits blob.
    /// The aggregator parses the header zero-copy and walks the trailing exits the same way the
    /// tx-processor's [`OutputCommitment`] does.
    ///
    /// The third tuple groups the per-bundle invariants the aggregator asserts every batch
    /// shares: `(lane_key, covenant_id, tx_image_id)`.
    ///
    /// [`OutputCommitment`]: crate::transaction_processor::OutputCommitment
    pub fn encode(
        w: &mut impl Writer,
        (prev_state, prev_lane_tip, prev_lane_blue_score): (&[u8; 32], &Hash, u64),
        (new_state, new_lane_tip, new_lane_blue_score): (&[u8; 32], &Hash, u64),
        (lane_key, covenant_id, tx_image_id): (&Hash, &[u8; 32], &[u8; 32]),
        lane_expired: bool,
        exits: &[u8],
    ) {
        // Fixed header.
        w.write(prev_state);
        w.write(new_state);
        w.write(prev_lane_tip.as_slice());
        w.write(new_lane_tip.as_slice());
        w.write(&prev_lane_blue_score.to_le_bytes());
        w.write(&new_lane_blue_score.to_le_bytes());
        w.write(lane_key.as_slice());
        w.write(covenant_id);
        w.write(tx_image_id);
        w.write(&[lane_expired as u8]);

        // Length-prefixed exits blob (same format as `OutputCommitment`'s exits arm).
        w.write_blob(exits);
    }

    /// Returns the trailing exits blob given the full journal bytes.
    pub fn exits(journal_bytes: &[u8]) -> crate::Result<ExitCommitment<'_>> {
        use vprogs_core_codec::Reader;
        let mut buf = journal_bytes
            .get(HEADER_SIZE..)
            .ok_or(crate::Error::Decode("journal shorter than BatchTransition header".into()))?;
        Ok(ExitCommitment::new(buf.blob("exits")?))
    }
}
