use crate::{Error, Result, Write};

/// Proven state transition for a batch - success or error (zero-copy on decode).
///
/// ## Wire format
///
/// ```text
/// discriminant(1) + payload
/// ```
///
/// Success payload is:
///
/// ```text
/// image_id(32) | prev_root(32) | new_root(32)
///   | lane_key(32) | parent_lane_tip(32) | new_lane_tip(32)
///   | block_hash(32) | blue_score(8) | daa_score(8)
///   | timestamp(8) | selected_parent_timestamp(8)
/// ```
///
/// Error payload is the encoded `Error`.
///
/// The raw seq-commit context fields (block_hash, blue_score, daa_score, timestamp, and
/// selected_parent_timestamp) are exposed so a covenant script on L1 can reconstruct the kip21
/// `mergeset_context_hash` independently rather than trusting a pre-computed hash.
pub enum StateTransition<'a> {
    /// Batch verified successfully.
    Success {
        /// Transaction processor guest image ID that was verified.
        image_id: &'a [u8; 32],
        /// State root before this batch was applied.
        prev_root: &'a [u8; 32],
        /// State root after this batch was applied.
        new_root: &'a [u8; 32],
        /// `H_lane_key(subnetwork_id)` for the lane this batch commits against.
        lane_key: &'a [u8; 32],
        /// Lane tip the batch advanced from.
        parent_lane_tip: &'a [u8; 32],
        /// Lane tip after applying this batch's activity digest + mergeset context.
        new_lane_tip: [u8; 32],
        /// L1 block hash the batch's transactions are drawn from.
        block_hash: &'a [u8; 32],
        /// DAG blue score of the block.
        blue_score: u64,
        /// DAA score of the block.
        daa_score: u64,
        /// Block header timestamp (milliseconds).
        timestamp: u64,
        /// Selected-parent timestamp (milliseconds) - the value kip21 `mergeset_context_hash`
        /// commits to.
        selected_parent_timestamp: u64,
    },
    /// Batch verification failed.
    Error(Error),
}

/// Inputs passed to [`StateTransition::encode`] for a successful batch.
pub struct SuccessInputs<'a> {
    /// Transaction processor guest image ID that was verified.
    pub image_id: &'a [u8; 32],
    /// State root before this batch was applied.
    pub prev_root: [u8; 32],
    /// State root after this batch was applied.
    pub new_root: [u8; 32],
    /// Lane key this batch binds to.
    pub lane_key: &'a [u8; 32],
    /// Lane tip the batch advanced from.
    pub parent_lane_tip: &'a [u8; 32],
    /// Lane tip after applying this batch's activity digest.
    pub new_lane_tip: [u8; 32],
    /// L1 block hash.
    pub block_hash: &'a [u8; 32],
    /// DAG blue score.
    pub blue_score: u64,
    /// DAA score.
    pub daa_score: u64,
    /// Block timestamp.
    pub timestamp: u64,
    /// Selected-parent timestamp.
    pub selected_parent_timestamp: u64,
}

impl<'a> StateTransition<'a> {
    /// Wire discriminant for a successful batch.
    const SUCCESS: u8 = 0x00;
    /// Wire discriminant for a failed batch.
    const ERROR: u8 = 0x01;

    /// Encodes a batch result to the journal (guest-side).
    pub fn encode(w: &mut impl Write, result: &Result<SuccessInputs<'_>>) {
        match result {
            Ok(s) => {
                w.write(&[Self::SUCCESS]);
                w.write(s.image_id);
                w.write(&s.prev_root);
                w.write(&s.new_root);
                w.write(s.lane_key);
                w.write(s.parent_lane_tip);
                w.write(&s.new_lane_tip);
                w.write(s.block_hash);
                w.write(&s.blue_score.to_le_bytes());
                w.write(&s.daa_score.to_le_bytes());
                w.write(&s.timestamp.to_le_bytes());
                w.write(&s.selected_parent_timestamp.to_le_bytes());
            }
            Err(error) => {
                w.write(&[Self::ERROR]);
                error.encode(w);
            }
        }
    }

    /// Decodes a state transition from a batch proof receipt (host-side, zero-copy).
    #[cfg(feature = "host")]
    pub fn decode(buf: &'a [u8]) -> Result<Self> {
        use vprogs_core_codec::Reader;

        let mut buf = buf;
        match buf.byte("discriminant")? {
            Self::SUCCESS => Ok(Self::Success {
                image_id: buf.array::<32>("image_id")?,
                prev_root: buf.array::<32>("prev_root")?,
                new_root: buf.array::<32>("new_root")?,
                lane_key: buf.array::<32>("lane_key")?,
                parent_lane_tip: buf.array::<32>("parent_lane_tip")?,
                new_lane_tip: *buf.array::<32>("new_lane_tip")?,
                block_hash: buf.array::<32>("block_hash")?,
                blue_score: buf.le_u64("blue_score")?,
                daa_score: buf.le_u64("daa_score")?,
                timestamp: buf.le_u64("timestamp")?,
                selected_parent_timestamp: buf.le_u64("selected_parent_timestamp")?,
            }),
            Self::ERROR => Ok(Self::Error(Error::decode(&mut buf)?)),
            _ => Err(Error::Decode("invalid state transition discriminant".into())),
        }
    }
}
