#[cfg(feature = "host")]
use vprogs_core_codec::Reader;

use crate::Write;
#[cfg(feature = "host")]
use crate::{Error, Result};

/// Bundle state transition journal (exactly 224 bytes).
pub struct StateTransition<'a> {
    /// State root before the bundle.
    pub prev_state: &'a [u8; 32],
    /// Lane tip entering the bundle.
    pub prev_lane_tip: &'a [u8; 32],
    /// State root after the bundle.
    pub new_state: &'a [u8; 32],
    /// Lane tip after the bundle.
    pub new_lane_tip: &'a [u8; 32],
    /// Block-header `seq_commit` derived from `new_lane_tip` and the lane proof.
    pub new_seq_commit: &'a [u8; 32],
    /// Covenant id this settlement binds to.
    pub covenant_id: &'a [u8; 32],
    /// Transaction-processor image id this settlement binds to.
    pub tx_image_id: &'a [u8; 32],
}

impl<'a> StateTransition<'a> {
    /// Wire size of the emitted journal.
    pub const SIZE: usize = 7 * 32;

    /// Decodes a state transition journal from bytes.
    #[cfg(feature = "host")]
    pub fn decode(mut buf: &'a [u8]) -> Result<Self> {
        if buf.len() != Self::SIZE {
            return Err(Error::Decode("batch journal must be exactly 224 bytes".into()));
        }
        Ok(Self {
            prev_state: buf.array::<32>("prev_state")?,
            prev_lane_tip: buf.array::<32>("prev_lane_tip")?,
            new_state: buf.array::<32>("new_state")?,
            new_lane_tip: buf.array::<32>("new_lane_tip")?,
            new_seq_commit: buf.array::<32>("new_seq_commit")?,
            covenant_id: buf.array::<32>("covenant_id")?,
            tx_image_id: buf.array::<32>("tx_image_id")?,
        })
    }

    /// Writes the state transition journal to `w`.
    pub fn encode(
        w: &mut impl Write,
        (prev_state, prev_lane_tip): (&[u8; 32], &[u8; 32]),
        (new_state, new_lane_tip, new_seq_commit): (&[u8; 32], &[u8; 32], &[u8; 32]),
        covenant_id: &[u8; 32],
        tx_image_id: &[u8; 32],
    ) {
        w.write(prev_state);
        w.write(prev_lane_tip);
        w.write(new_state);
        w.write(new_lane_tip);
        w.write(new_seq_commit);
        w.write(covenant_id);
        w.write(tx_image_id);
    }
}
