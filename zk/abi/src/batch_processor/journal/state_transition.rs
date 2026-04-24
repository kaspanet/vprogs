use crate::{Error, Result, Write};

/// Proven state transition journal — exactly 160 bytes, SHA-256's to the value the covenant
/// script reconstructs on-stack via `OpChainblockSeqCommit` + data pushes, then checks via
/// `OpZkPrecompile`.
///
/// Wire format (no discriminant):
///
/// ```text
/// prev_state(32) | prev_seq(32) | new_state(32) | new_seq(32) | covenant_id(32)
/// ```
///
/// Failed batches do not produce a journal: the guest panics on error, no receipt is emitted.
pub struct StateTransition<'a> {
    /// State root before this batch.
    pub prev_state: [u8; 32],
    /// Sequencing commitment entering the batch.
    pub prev_seq: &'a [u8; 32],
    /// State root after this batch.
    pub new_state: [u8; 32],
    /// Sequencing commitment after the batch (covenant cross-checks against
    /// `OpChainblockSeqCommit(block_prove_to)`).
    pub new_seq: &'a [u8; 32],
    /// Covenant id the settlement binds to.
    pub covenant_id: &'a [u8; 32],
}

impl<'a> StateTransition<'a> {
    /// Wire size of the emitted journal.
    pub const SIZE: usize = 32 * 5;

    /// Encodes the state transition to the journal (guest-side).
    pub fn encode(w: &mut impl Write, s: &Self) {
        w.write(&s.prev_state);
        w.write(s.prev_seq);
        w.write(&s.new_state);
        w.write(s.new_seq);
        w.write(s.covenant_id);
    }

    /// Decodes a state transition from a batch proof receipt (host-side, zero-copy).
    #[cfg(feature = "host")]
    pub fn decode(buf: &'a [u8]) -> Result<Self> {
        use vprogs_core_codec::Reader;

        if buf.len() != Self::SIZE {
            return Err(Error::Decode("batch journal must be exactly 160 bytes".into()));
        }
        let mut buf = buf;
        Ok(Self {
            prev_state: *buf.array::<32>("prev_state")?,
            prev_seq: buf.array::<32>("prev_seq")?,
            new_state: *buf.array::<32>("new_state")?,
            new_seq: buf.array::<32>("new_seq")?,
            covenant_id: buf.array::<32>("covenant_id")?,
        })
    }
}
