use vprogs_core_codec::{Reader, Writer};

use crate::{
    Result,
    transaction_processor::{InputCommitment, Inputs, OutputCommitment, Resource},
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

    /// Decodes a single journal entry from `buf`.
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

    /// Encodes an input commitment journal entry: TLV header + payload.
    pub fn encode_input(w: &mut impl Writer, inputs: &Inputs<'_>) {
        w.write(&[Self::OPCODE_INPUT]);
        w.write(&(InputCommitment::wire_size(inputs) as u32).to_le_bytes());
        InputCommitment::encode(w, inputs);
    }

    /// Encodes an output commitment journal entry: TLV header + payload.
    pub fn encode_output(w: &mut impl Writer, result: &Result<&[Resource<'_>]>) {
        w.write(&[Self::OPCODE_OUTPUT]);
        w.write(&(OutputCommitment::wire_size(result) as u32).to_le_bytes());
        OutputCommitment::encode(w, result);
    }
}
