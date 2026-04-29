use crate::Write;
#[cfg(feature = "host")]
use crate::{Error, Result};

/// Proven state transition journal — exactly 224 bytes, SHA-256's to the value the covenant
/// script reconstructs on-stack via `OpChainblockSeqCommit` + data pushes, then checks via
/// `OpZkPrecompile`.
///
/// Wire format (no discriminant):
///
/// ```text
/// prev_state(32) | prev_lane_tip(32) | new_state(32) | new_lane_tip(32)
///   | new_seq_commit(32) | covenant_id(32) | tx_image_id(32)
/// ```
///
/// `prev_lane_tip` is the UTXO-locked rollup state the covenant's redeem prefix embeds; the
/// guest echoes it so the journal hash binds it. `new_lane_tip` is what the covenant's
/// redeem prefix will carry into the next spend; `new_seq_commit` is the derived block-header
/// anchor checked against `OpChainblockSeqCommit(block_prove_to)`. `tx_image_id` is the
/// transaction-processor guest image id the batch guest used to verify each inner tx receipt
/// — echoed from input so the covenant can pin it via a hardcoded data constant in the redeem
/// script. Without this field, the batch guest's `env::verify` would accept any host-supplied
/// inner image id, including a backdoored verifier.
///
/// Failed batches do not produce a journal: the guest panics on error, no receipt is emitted.
pub struct StateTransition<'a> {
    /// State root before this batch.
    pub prev_state: [u8; 32],
    /// Lane tip entering this batch — echoed from input; matches the covenant UTXO's redeem
    /// prefix so the on-chain P2SH check binds it.
    pub prev_lane_tip: &'a [u8; 32],
    /// State root after this batch.
    pub new_state: [u8; 32],
    /// Lane tip after applying this batch — locked into the next covenant UTXO's redeem
    /// prefix. Owned: guest-derived from `lane_tip_next`.
    pub new_lane_tip: [u8; 32],
    /// Block-header seq-commit derived from `new_lane_tip` + lane-proof ingredients. Covenant
    /// cross-checks against `OpChainblockSeqCommit(block_prove_to)`. Owned.
    pub new_seq_commit: [u8; 32],
    /// Covenant id the settlement binds to.
    pub covenant_id: &'a [u8; 32],
    /// Transaction-processor image id the batch guest verified each inner tx receipt against.
    /// Echoed from input so the covenant can compare against a hardcoded constant in the
    /// redeem script — pinning the tx-processor identity transitively through the journal hash.
    pub tx_image_id: &'a [u8; 32],
}

impl<'a> StateTransition<'a> {
    /// Wire size of the emitted journal.
    pub const SIZE: usize = 32 * 7;

    /// Encodes the state transition to the journal (guest-side).
    pub fn encode(w: &mut impl Write, s: &Self) {
        w.write(&s.prev_state);
        w.write(s.prev_lane_tip);
        w.write(&s.new_state);
        w.write(&s.new_lane_tip);
        w.write(&s.new_seq_commit);
        w.write(s.covenant_id);
        w.write(s.tx_image_id);
    }

    /// Decodes a state transition from a batch proof receipt (host-side, zero-copy).
    #[cfg(feature = "host")]
    pub fn decode(buf: &'a [u8]) -> Result<Self> {
        use vprogs_core_codec::Reader;

        if buf.len() != Self::SIZE {
            return Err(Error::Decode("batch journal must be exactly 224 bytes".into()));
        }
        let mut buf = buf;
        Ok(Self {
            prev_state: *buf.array::<32>("prev_state")?,
            prev_lane_tip: buf.array::<32>("prev_lane_tip")?,
            new_state: *buf.array::<32>("new_state")?,
            new_lane_tip: *buf.array::<32>("new_lane_tip")?,
            new_seq_commit: *buf.array::<32>("new_seq_commit")?,
            covenant_id: buf.array::<32>("covenant_id")?,
            tx_image_id: buf.array::<32>("tx_image_id")?,
        })
    }
}
