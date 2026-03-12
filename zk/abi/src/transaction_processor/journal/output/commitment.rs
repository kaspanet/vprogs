use vprogs_zk_smt::EMPTY_LEAF_HASH;

use crate::{
    Error, Parser, Result, Write,
    transaction_processor::{
        JournalEntry, OutputResourceCommitment, OutputResourceCommitments, Resource,
    },
};

/// Decoded output commitment from a transaction processor journal.
pub enum OutputCommitment<'a> {
    /// Transaction executed successfully; contains per-resource output commitments.
    Success(OutputResourceCommitments<'a>),
    /// Transaction execution failed; contains the numeric error code.
    Error(u32),
}

impl<'a> OutputCommitment<'a> {
    /// Wire discriminant for a successful execution.
    pub const SUCCESS: u8 = 0x00;
    /// Wire discriminant for a failed execution.
    pub const ERROR: u8 = 0x01;

    /// Decodes an output commitment from a journal segment payload.
    pub fn decode(payload: &'a [u8]) -> Result<Self> {
        // Dispatch based on discriminant.
        match payload[0] {
            Self::SUCCESS => Ok(Self::Success(OutputResourceCommitments::new(&payload[1..]))),
            Self::ERROR => Ok(Self::Error(payload[1..5].parse_u32("error_code")?)),
            _ => Err(Error::Decode("invalid output commitment discriminant")),
        }
    }

    /// Encodes an output commitment segment to the journal (guest-side).
    pub fn encode(w: &mut impl Write, result: Result<&[Resource<'_>]>) {
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
                w.write(&err.code().to_le_bytes());
            }
        }
    }
}
