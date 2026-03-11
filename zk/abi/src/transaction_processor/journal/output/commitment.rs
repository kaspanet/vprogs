use vprogs_zk_smt::EMPTY_LEAF_HASH;

use crate::{
    Write,
    transaction_processor::{
        JournalEntry, OutputResourceCommitment, OutputResourceCommitments, Resource,
    },
};

/// Decoded output commitment from a transaction processor journal.
pub enum OutputCommitment<'a> {
    Success { outputs: OutputResourceCommitments<'a> },
    Error { error_code: u32 },
}

impl<'a> OutputCommitment<'a> {
    pub const SUCCESS: u8 = 0x00;
    pub const ERROR: u8 = 0x01;

    /// Decodes an output commitment from a journal segment payload.
    pub fn decode(payload: &'a [u8]) -> Self {
        match payload[0] {
            Self::SUCCESS => {
                Self::Success { outputs: OutputResourceCommitments { buf: payload, offset: 1 } }
            }
            Self::ERROR => {
                let error_code = u32::from_le_bytes(payload[1..5].try_into().unwrap());
                Self::Error { error_code }
            }
            other => panic!("invalid output commitment discriminant: {other}"),
        }
    }

    /// Guest-side: encode an output commitment segment to the journal.
    pub fn encode(w: &mut impl Write, result: crate::Result<&[Resource<'_>]>) {
        match result {
            Ok(resources) => {
                // Calculate payload: discriminant(1) + per-resource flags and optional hashes.
                let payload_len: usize = 1 + resources
                    .iter()
                    .map(|r| if r.is_dirty() || r.is_deleted() { 33 } else { 1 })
                    .sum::<usize>();

                // Segment header: opcode + payload length.
                w.write(&[JournalEntry::OPCODE_OUTPUT]);
                w.write(&(payload_len as u32).to_le_bytes());

                // Success discriminant.
                w.write(&[Self::SUCCESS]);

                // Per-resource output commitments.
                for r in resources {
                    if r.is_deleted() {
                        w.write(&[OutputResourceCommitment::CHANGED]);
                        w.write(&EMPTY_LEAF_HASH);
                    } else if r.is_dirty() {
                        w.write(&[OutputResourceCommitment::CHANGED]);
                        w.write(blake3::hash(r.data()).as_bytes());
                    } else {
                        w.write(&[OutputResourceCommitment::UNCHANGED]);
                    }
                }
            }
            Err(err) => {
                // Segment header: opcode + payload length.
                w.write(&[JournalEntry::OPCODE_OUTPUT]);
                w.write(&5u32.to_le_bytes());

                // Error discriminant + error code.
                w.write(&[Self::ERROR]);
                w.write(&err.0.to_le_bytes());
            }
        }
    }
}
