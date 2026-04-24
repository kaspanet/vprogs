//! Settlement journal layout - the public input committed by the settlement guest.
//!
//! The covenant script rebuilds this preimage on-stack using stack-stashed prev values, the
//! block's sequencing commitment, the new application state supplied in the witness, and the input
//! covenant id surfaced by `OpInputCovenantId`. Hashing it with SHA-256 yields the `journal_hash`
//! consumed by `OpZkPrecompile`, so the guest's committed bytes must match this layout byte for
//! byte.

/// Journal preimage size in bytes (matches biryukovmaxim's `zk-covenant-rollup` base preimage).
pub const JOURNAL_SIZE: usize = 160;

/// Parsed view over a 160-byte settlement journal.
///
/// Wire layout:
///
/// ```text
/// prev_state(32) | prev_seq(32) | new_state(32) | new_seq(32) | covenant_id(32)
/// ```
pub struct SettlementJournal<'a> {
    /// L2 state root before this batch.
    pub prev_state: &'a [u8; 32],
    /// Lane tip entering this batch (kip21 seq commitment).
    pub prev_seq: &'a [u8; 32],
    /// L2 state root after this batch.
    pub new_state: &'a [u8; 32],
    /// Lane tip after applying this batch.
    pub new_seq: &'a [u8; 32],
    /// Covenant id bound to the settlement UTXO.
    pub covenant_id: &'a [u8; 32],
}

impl<'a> SettlementJournal<'a> {
    /// Parses a journal buffer. Returns `None` if the buffer is not exactly [`JOURNAL_SIZE`] bytes.
    pub fn decode(buf: &'a [u8]) -> Option<Self> {
        let bytes: &[u8; JOURNAL_SIZE] = buf.try_into().ok()?;
        let (prev_state, rest) = bytes.split_first_chunk::<32>()?;
        let (prev_seq, rest) = rest.split_first_chunk::<32>()?;
        let (new_state, rest) = rest.split_first_chunk::<32>()?;
        let (new_seq, covenant_id) = rest.split_first_chunk::<32>()?;
        let covenant_id: &[u8; 32] = covenant_id.try_into().ok()?;
        Some(Self { prev_state, prev_seq, new_state, new_seq, covenant_id })
    }

    /// Encodes the five 32-byte fields into a fresh [`JOURNAL_SIZE`]-byte buffer.
    pub fn encode(
        prev_state: &[u8; 32],
        prev_seq: &[u8; 32],
        new_state: &[u8; 32],
        new_seq: &[u8; 32],
        covenant_id: &[u8; 32],
    ) -> [u8; JOURNAL_SIZE] {
        let mut buf = [0u8; JOURNAL_SIZE];
        buf[0..32].copy_from_slice(prev_state);
        buf[32..64].copy_from_slice(prev_seq);
        buf[64..96].copy_from_slice(new_state);
        buf[96..128].copy_from_slice(new_seq);
        buf[128..160].copy_from_slice(covenant_id);
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let prev_state = [1u8; 32];
        let prev_seq = [2u8; 32];
        let new_state = [3u8; 32];
        let new_seq = [4u8; 32];
        let covenant_id = [5u8; 32];

        let encoded =
            SettlementJournal::encode(&prev_state, &prev_seq, &new_state, &new_seq, &covenant_id);
        let decoded = SettlementJournal::decode(&encoded).expect("valid journal");

        assert_eq!(decoded.prev_state, &prev_state);
        assert_eq!(decoded.prev_seq, &prev_seq);
        assert_eq!(decoded.new_state, &new_state);
        assert_eq!(decoded.new_seq, &new_seq);
        assert_eq!(decoded.covenant_id, &covenant_id);
    }

    #[test]
    fn decode_rejects_wrong_size() {
        assert!(SettlementJournal::decode(&[0u8; 159]).is_none());
        assert!(SettlementJournal::decode(&[0u8; 161]).is_none());
    }
}
