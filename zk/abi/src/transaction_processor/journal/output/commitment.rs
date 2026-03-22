use vprogs_core_codec::Reader;
use vprogs_core_smt::EMPTY_HASH;

use crate::{
    Error, Result, Write,
    transaction_processor::{
        JournalEntry, OutputResourceCommitment, OutputResourceCommitments, Resource,
    },
};

/// Decoded output commitment from a transaction processor journal.
pub enum OutputCommitment<'a> {
    /// Transaction executed successfully; contains per-resource output commitments.
    Success(OutputResourceCommitments<'a>),
    /// Transaction execution failed; contains the error.
    Error(Error),
}

impl<'a> OutputCommitment<'a> {
    /// Wire discriminant for a successful execution.
    pub const SUCCESS: u8 = 0x00;
    /// Wire discriminant for a failed execution.
    pub const ERROR: u8 = 0x01;

    /// Decodes an output commitment from a journal segment payload.
    pub fn decode(mut buf: &'a [u8]) -> Result<Self> {
        match buf.byte("discriminant")? {
            Self::SUCCESS => Ok(Self::Success(OutputResourceCommitments::new(buf))),
            Self::ERROR => Ok(Self::Error(Error::decode(&mut buf)?)),
            _ => Err(Error::Decode("invalid output commitment discriminant".into())),
        }
    }

    /// Encodes an output commitment segment to the journal (guest-side).
    pub fn encode(w: &mut impl Write, result: &Result<&[Resource<'_>]>) {
        match *result {
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
                        w.write(&EMPTY_HASH);
                    } else if r.is_dirty() {
                        w.write(&[OutputResourceCommitment::CHANGED]);
                        w.write(blake3::hash(r.data()).as_bytes());
                    } else {
                        w.write(&[OutputResourceCommitment::UNCHANGED]);
                    }
                }
            }
            Err(ref err) => {
                // Segment header: opcode + payload length.
                w.write(&[JournalEntry::OPCODE_OUTPUT]);
                w.write(&((1 + err.wire_size()) as u32).to_le_bytes());

                // Error discriminant + encoded error.
                w.write(&[Self::ERROR]);
                err.encode(w);
            }
        }
    }
}
