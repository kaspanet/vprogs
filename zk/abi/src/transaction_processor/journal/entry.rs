use vprogs_core_utils::Parser;

use crate::{
    Result,
    transaction_processor::{InputCommitment, OutputCommitment},
};

/// A single decoded segment from a transaction processor journal.
pub enum JournalEntry<'a> {
    /// Input commitment segment.
    Input(InputCommitment<'a>),
    /// Output commitment segment.
    Output(OutputCommitment<'a>),
    /// Unrecognized segment with its opcode and raw payload.
    Unknown(u8, &'a [u8]),
}

impl<'a> JournalEntry<'a> {
    /// Opcode identifying the input commitment segment.
    pub const OPCODE_INPUT: u8 = 0x01;
    /// Opcode identifying the output commitment segment (sentinel, always last).
    pub const OPCODE_OUTPUT: u8 = 0xFF;

    /// Decodes a single journal entry, advancing `buf` past the consumed bytes.
    ///
    /// Wire layout per entry: `opcode(1) + payload_len(4) + payload(N)`.
    pub fn decode(buf: &mut &'a [u8]) -> Result<Self> {
        // Parse TLV header.
        let opcode = buf.byte("opcode")?;
        let payload_length = buf.le_u32("payload_length")? as usize;
        let payload = buf.bytes(payload_length, "payload")?;

        // Dispatch to segment decoder.
        match opcode {
            Self::OPCODE_INPUT => Ok(Self::Input(InputCommitment::decode(payload)?)),
            Self::OPCODE_OUTPUT => Ok(Self::Output(OutputCommitment::decode(payload)?)),
            _ => Ok(Self::Unknown(opcode, payload)),
        }
    }
}
